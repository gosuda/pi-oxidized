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

use std::fs::{self, File, OpenOptions};
use std::io;
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
    /// An external import would replace an existing session with the same basename.
    #[error("A session already exists at {0}")]
    ImportCollision(String),
    /// Factory failed to build the replacement runtime.
    #[error("runtime replacement failed: {0}")]
    Factory(String),
}

// ---------------------------------------------------------------------------
// AgentSessionRuntime
// ---------------------------------------------------------------------------

type RemoveFileFn = Arc<dyn Fn(&Path) -> io::Result<()> + Send + Sync>;

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
    remove_file: RwLock<RemoveFileFn>,
    diagnostics:
        Arc<RwLock<Vec<crate::core::agent_session_services::AgentSessionRuntimeDiagnostic>>>,
    model_fallback_message: RwLock<Option<String>>,
    rebind_session: RwLock<Option<RebindSessionCallback>>,
    before_session_invalidate: RwLock<Option<BeforeSessionInvalidateCallback>>,
    /// Serializes all replacement operations (new / switch / fork / import / dispose).
    replacement_lock: Arc<AsyncMutex<()>>,
    #[cfg(test)]
    import_commit_gate: RwLock<Option<Arc<tokio::sync::Semaphore>>>,
    #[cfg(test)]
    import_commit_started: tokio::sync::Notify,
    #[cfg(test)]
    import_commit_finished: tokio::sync::Notify,
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
            remove_file: RwLock::new(Arc::new(|path: &Path| fs::remove_file(path))),
            diagnostics: Arc::new(RwLock::new(diagnostics)),
            model_fallback_message: RwLock::new(model_fallback_message),
            rebind_session: RwLock::new(None),
            before_session_invalidate: RwLock::new(None),
            replacement_lock: Arc::new(AsyncMutex::new(())),
            #[cfg(test)]
            import_commit_gate: RwLock::new(None),
            #[cfg(test)]
            import_commit_started: tokio::sync::Notify::new(),
            #[cfg(test)]
            import_commit_finished: tokio::sync::Notify::new(),
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
        self: &Arc<Self>,
        input_path: &str,
        cwd_override: Option<&str>,
    ) -> Result<SwitchOutcome, AgentSessionRuntimeError> {
        let replacement_guard = Arc::clone(&self.replacement_lock).lock_owned().await;
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
        let paths = prepare_import_paths(Path::new(&resolved), &session_dir)?;

        let before = self
            .emit_before_switch(
                SessionStartReason::Resume,
                Some(paths.destination_text.as_str()),
            )
            .await;
        if before.cancelled {
            return Ok(before);
        }

        if paths.same_file {
            let result = self
                .create_import_replacement(&paths.destination_text, &session_dir, cwd_override)
                .await?;
            self.teardown_current(SessionShutdownReason::Resume, Some(&paths.destination_text))
                .await;
            self.apply(result);
            self.finish_session_replacement(None).await;
            return Ok(SwitchOutcome { cancelled: false });
        }

        let file_name = paths.source.file_name().map_or_else(
            || "imported.jsonl".to_owned(),
            |name| name.to_string_lossy().into_owned(),
        );
        let mut staged_import = StagedImport::new(
            stage_import_file(&session_dir, &file_name, &paths.source)?,
            self.import_remove_file(),
            Arc::clone(&self.diagnostics),
        );
        let staged_text = staged_import.path().to_string_lossy().into_owned();
        let mut result = match self
            .create_import_replacement(&staged_text, &session_dir, cwd_override)
            .await
        {
            Ok(result) => result,
            Err(error) => return Err(error),
        };

        let publication = match publish_staged_import(
            staged_import.path(),
            &paths.destination,
            &paths.destination_text,
        ) {
            Ok(publication) => publication,
            Err(error) => {
                result.session.dispose().await;
                return Err(error);
            }
        };
        let cleanup_warning = match publication {
            ImportPublication::Linked => {
                staged_import.cleanup("Imported session but failed to remove staging file")
            }
            ImportPublication::Moved => {
                staged_import.disarm();
                None
            }
        };
        if let Some(diagnostic) = cleanup_warning {
            result.diagnostics.push(diagnostic);
        }

        let runtime = Arc::clone(self);
        let destination_text = paths.destination_text;
        tokio::spawn(async move {
            runtime
                .commit_import(replacement_guard, result, destination_text)
                .await
        })
        .await
        .map_err(|error| {
            AgentSessionRuntimeError::Factory(format!("import commit task failed: {error}"))
        })
    }

    async fn commit_import(
        &self,
        replacement_guard: tokio::sync::OwnedMutexGuard<()>,
        result: CreateAgentSessionRuntimeResult,
        destination_text: String,
    ) -> SwitchOutcome {
        let _replacement_guard = replacement_guard;
        #[cfg(test)]
        {
            self.import_commit_started.notify_one();
            let gate = self.import_commit_gate.read().map_or_else(
                |poisoned| poisoned.into_inner().clone(),
                |current| current.clone(),
            );
            if let Some(gate) = gate
                && let Ok(permit) = gate.acquire_owned().await
            {
                permit.forget();
            }
        }

        let replacement_manager = result.session.session_manager();
        replacement_manager
            .lock()
            .await
            .rebind_session_file_after_atomic_move(destination_text.clone());
        self.teardown_current(SessionShutdownReason::Resume, Some(&destination_text))
            .await;
        self.apply(result);
        self.finish_session_replacement(None).await;
        #[cfg(test)]
        self.import_commit_finished.notify_one();
        SwitchOutcome { cancelled: false }
    }

    /// Dispose the current session and the runtime.
    pub async fn dispose(&self) {
        let _guard = self.replacement_lock.lock().await;
        self.teardown_current(SessionShutdownReason::Quit, None)
            .await;
    }

    // ----- Internal helpers ----------------------------------------------

    fn import_remove_file(&self) -> RemoveFileFn {
        self.remove_file.read().map_or_else(
            |poisoned| Arc::clone(&*poisoned.into_inner()),
            |handler| Arc::clone(&*handler),
        )
    }

    #[cfg(test)]
    fn set_remove_file_for_test(&self, remove_file: RemoveFileFn) {
        if let Ok(mut handler) = self.remove_file.write() {
            *handler = remove_file;
        }
    }

    #[cfg(test)]
    fn set_import_commit_gate(&self, gate: Arc<tokio::sync::Semaphore>) {
        if let Ok(mut current) = self.import_commit_gate.write() {
            *current = Some(gate);
        }
    }

    #[cfg(test)]
    async fn wait_for_import_commit_started(&self) {
        self.import_commit_started.notified().await;
    }

    #[cfg(test)]
    async fn wait_for_import_commit_finished(&self) {
        self.import_commit_finished.notified().await;
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
        // Pre-invalidate callback (host UI teardown, sync). Falls back to the
        // session-bound shutdown handler when no runtime-level callback exists.
        let runtime_cb = self
            .before_session_invalidate
            .read()
            .ok()
            .and_then(|g| g.clone());
        if let Some(cb) = runtime_cb {
            cb();
        } else {
            session.invoke_extension_shutdown_handler();
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

    async fn create_import_replacement(
        &self,
        session_file: &str,
        session_dir: &str,
        cwd_override: Option<&str>,
    ) -> Result<CreateAgentSessionRuntimeResult, AgentSessionRuntimeError> {
        let session_manager = SessionManager::open(session_file, Some(session_dir), cwd_override)?;
        self.assert_cwd(&session_manager)?;
        let new_cwd = session_manager.get_cwd().to_owned();
        self.factory
            .create(CreateAgentSessionRuntimeOptions {
                cwd: new_cwd.clone(),
                agent_dir: self.agent_dir(),
                session_manager,
                start_reason: SessionStartReason::Resume,
                previous_session_file: self.session_file_for_teardown().await,
            })
            .await
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

struct ImportPaths {
    source: PathBuf,
    destination: PathBuf,
    destination_text: String,
    same_file: bool,
}
struct StagedImport {
    path: PathBuf,
    remove_file: RemoveFileFn,
    diagnostics:
        Arc<RwLock<Vec<crate::core::agent_session_services::AgentSessionRuntimeDiagnostic>>>,
    armed: bool,
}

impl StagedImport {
    fn new(
        path: PathBuf,
        remove_file: RemoveFileFn,
        diagnostics: Arc<
            RwLock<Vec<crate::core::agent_session_services::AgentSessionRuntimeDiagnostic>>,
        >,
    ) -> Self {
        Self {
            path,
            remove_file,
            diagnostics,
            armed: true,
        }
    }

    fn path(&self) -> &Path {
        &self.path
    }

    fn cleanup(
        &mut self,
        failure_context: &str,
    ) -> Option<crate::core::agent_session_services::AgentSessionRuntimeDiagnostic> {
        if !self.armed {
            return None;
        }
        self.armed = false;
        (self.remove_file)(&self.path).err().map(|error| {
            crate::core::agent_session_services::AgentSessionRuntimeDiagnostic {
                kind:
                    crate::core::agent_session_services::AgentSessionRuntimeDiagnosticKind::Warning,
                message: format!("{failure_context} {}: {error}", self.path.display()),
            }
        })
    }

    fn disarm(&mut self) {
        self.armed = false;
    }
}

impl Drop for StagedImport {
    fn drop(&mut self) {
        if let Some(diagnostic) = self.cleanup("Import stopped but failed to remove staging file")
            && let Ok(mut diagnostics) = self.diagnostics.write()
        {
            diagnostics.push(diagnostic);
        }
    }
}

fn prepare_import_paths(
    resolved: &Path,
    session_dir: &str,
) -> Result<ImportPaths, AgentSessionRuntimeError> {
    if !Path::new(session_dir).exists() {
        fs::create_dir_all(session_dir)
            .map_err(|error| AgentSessionRuntimeError::Transfer(error.to_string()))?;
    }

    let file_name = resolved.file_name().map_or_else(
        || "imported.jsonl".to_owned(),
        |name| name.to_string_lossy().into_owned(),
    );
    let destination = Path::new(session_dir).join(&file_name);
    let destination_text = destination.to_string_lossy().into_owned();
    let resolved_canonical = fs::canonicalize(resolved)
        .map_err(|error| AgentSessionRuntimeError::Transfer(error.to_string()))?;
    let destination_exists = fs::symlink_metadata(&destination).is_ok();
    let same_file = destination_exists
        && fs::canonicalize(&destination)
            .is_ok_and(|destination_canonical| destination_canonical == resolved_canonical);

    if destination_exists && !same_file {
        return Err(AgentSessionRuntimeError::ImportCollision(destination_text));
    }

    Ok(ImportPaths {
        source: resolved.to_path_buf(),
        destination,
        destination_text,
        same_file,
    })
}

fn stage_import_file(
    session_dir: &str,
    file_name: &str,
    source_path: &Path,
) -> Result<PathBuf, AgentSessionRuntimeError> {
    loop {
        let candidate =
            Path::new(session_dir).join(format!(".{file_name}.{}.tmp", uuid::Uuid::new_v4()));
        let mut staged = match OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&candidate)
        {
            Ok(file) => file,
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => continue,
            Err(error) => return Err(AgentSessionRuntimeError::Transfer(error.to_string())),
        };
        let mut source = match File::open(source_path) {
            Ok(source) => source,
            Err(error) => {
                let _ = fs::remove_file(&candidate);
                return Err(AgentSessionRuntimeError::Transfer(error.to_string()));
            }
        };
        let copied = io::copy(&mut source, &mut staged)
            .map_err(|error| AgentSessionRuntimeError::Transfer(error.to_string()));
        let synced = staged
            .sync_all()
            .map_err(|error| AgentSessionRuntimeError::Transfer(error.to_string()));
        drop(staged);
        if let Err(error) = copied {
            let _ = fs::remove_file(&candidate);
            return Err(error);
        }
        if let Err(error) = synced {
            let _ = fs::remove_file(&candidate);
            return Err(error);
        }
        break Ok(candidate);
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum ImportPublication {
    Linked,
    Moved,
}

fn publish_no_replace(staged_path: &Path, destination: &Path) -> io::Result<ImportPublication> {
    publish_no_replace_with(
        staged_path,
        destination,
        |source: &Path, target: &Path| fs::hard_link(source, target),
        atomic_move_noreplace,
    )
}

fn publish_no_replace_with(
    staged_path: &Path,
    destination: &Path,
    link: impl Fn(&Path, &Path) -> io::Result<()>,
    move_noreplace: impl Fn(&Path, &Path) -> io::Result<()>,
) -> io::Result<ImportPublication> {
    match link(staged_path, destination) {
        Ok(()) => Ok(ImportPublication::Linked),
        Err(error) if error.kind() == io::ErrorKind::AlreadyExists => Err(error),
        Err(link_error) => match move_noreplace(staged_path, destination) {
            Ok(()) => Ok(ImportPublication::Moved),
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => Err(error),
            Err(_) => Err(link_error),
        },
    }
}

#[cfg(any(
    target_os = "linux",
    target_os = "android",
    target_vendor = "apple",
    target_os = "redox"
))]
fn atomic_move_noreplace(staged_path: &Path, destination: &Path) -> io::Result<()> {
    rustix::fs::renameat_with(
        rustix::fs::CWD,
        staged_path,
        rustix::fs::CWD,
        destination,
        rustix::fs::RenameFlags::NOREPLACE,
    )
    .map_err(io::Error::from)
}

#[cfg(not(any(
    target_os = "linux",
    target_os = "android",
    target_vendor = "apple",
    target_os = "redox"
)))]
fn atomic_move_noreplace(_staged_path: &Path, _destination: &Path) -> io::Result<()> {
    Err(io::Error::new(
        io::ErrorKind::Unsupported,
        "atomic no-replace rename is unavailable on this target",
    ))
}

fn publish_staged_import(
    staged_path: &Path,
    destination: &Path,
    destination_text: &str,
) -> Result<ImportPublication, AgentSessionRuntimeError> {
    match publish_no_replace(staged_path, destination) {
        Ok(publication) => Ok(publication),
        Err(error) if error.kind() == io::ErrorKind::AlreadyExists => Err(
            AgentSessionRuntimeError::ImportCollision(destination_text.to_owned()),
        ),
        Err(error) => Err(AgentSessionRuntimeError::Transfer(error.to_string())),
    }
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
mod tests;
