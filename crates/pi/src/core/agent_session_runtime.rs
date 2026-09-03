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
//!    emit, then lifecycle exclusion, `before_session_invalidate`, and disposal.
//! 5. `apply(result)`: swap session + services + diagnostics.
//! 6. `finish_session_replacement`: `rebind_session(new_session)` then
//!    optional `with_session(ctx)`. The mode's rebind calls `bind_extensions`
//!    on the new session, which emits the stored
//!    `session_start{new|resume|fork}` after the old host received its
//!    `session_shutdown` in step 4.

use std::collections::BTreeMap;
use std::fs::{self, File, OpenOptions};
use std::io;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex as StdMutex, RwLock, Weak};

use futures::future::BoxFuture;
use pi_ai::Model;
use tokio::sync::{Mutex as AsyncMutex, RwLock as AsyncRwLock};

use crate::core::agent_session::events::{
    AgentSessionEvent, SessionBeforeForkPosition, SessionBeforeSwitchReason, SessionShutdownReason,
    SessionStartReason,
};
use crate::core::agent_session::{AgentSession, ReplacedSessionContext};
use crate::core::agent_session_services::ExtensionFlagValue;
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
    /// Current model to preserve for a replacement, when available.
    pub model: Option<Model>,
    /// Live extension flag values snapshotted from the current session's
    /// runner before teardown. Only Bool/String values are carried; other
    /// JSON types are rejected at snapshot time.
    pub extension_flag_values: BTreeMap<String, ExtensionFlagValue>,
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
/// Synchronous callback invoked immediately before a bridge replacement starts
/// tearing down the current session.
pub type BeforeSessionReplacementCallback = Arc<dyn Fn() + Send + Sync>;

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

/// Result of preparing a replacement without tearing down the current session.
pub(crate) enum PrepareReplacementOutcome {
    /// A cancellable extension hook rejected the operation.
    Cancelled,
    /// A replacement is ready to install or abort.
    Prepared(PreparedReplacement),
}

/// A fully constructed replacement that has not touched the current session.
///
/// The `result` field is an [`Option`] so that [`Drop`] can tell whether the
/// replacement was consumed by finalize / abort. If neither path runs (early
/// return, `?`, panic-unwind), `Drop` reaps the live session on a background
/// task — matching the `PreparedReload` guard in `extension_runtime_set/planning.rs`.
pub(crate) struct PreparedReplacement {
    pub(crate) result: Option<CreateAgentSessionRuntimeResult>,
    pub(crate) reason: SessionShutdownReason,
    pub(crate) target_session_file: Option<String>,
}

/// Spawn resource cleanup even when the caller has no entered Tokio runtime.
pub(crate) fn spawn_runtime_safe<F>(task_name: &'static str, future: F)
where
    F: Future<Output = ()> + Send + 'static,
{
    if let Ok(handle) = tokio::runtime::Handle::try_current() {
        handle.spawn(future);
        return;
    }
    let _ = std::thread::Builder::new()
        .name(task_name.to_owned())
        .spawn(move || {
            let runtime = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build();
            let Ok(runtime) = runtime else {
                return;
            };
            runtime.block_on(future);
        });
}

impl Drop for PreparedReplacement {
    fn drop(&mut self) {
        if let Some(result) = self.result.take() {
            spawn_runtime_safe("prepared-replacement-drop", async move {
                result.session.dispose().await;
            });
        }
    }
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
    /// Another replacement is waiting for its host command to finish.
    #[error("session replacement in progress")]
    ReplacementBusy,
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
    before_session_replacement: RwLock<Option<BeforeSessionReplacementCallback>>,
    /// Weak self used to link every current session back to this runtime.
    self_handle: StdMutex<Option<Weak<AgentSessionRuntime>>>,
    /// Serializes all replacement operations (new / switch / fork / import / dispose).
    replacement_lock: Arc<AsyncMutex<()>>,
    /// Excludes old-session tree navigation while teardown and apply run.
    lifecycle_gate: AsyncRwLock<()>,
    #[cfg(test)]
    import_commit_gate: RwLock<Option<Arc<tokio::sync::Semaphore>>>,
    #[cfg(test)]
    import_commit_started: tokio::sync::Notify,
    #[cfg(test)]
    import_commit_finished: tokio::sync::Notify,
    /// Test hook: fires after `navigate_tree` acquires the lifecycle read gate.
    #[cfg(test)]
    tree_read_gate_acquired: tokio::sync::Notify,
    /// Test hook: `navigate_tree` awaits this before proceeding past the read gate.
    #[cfg(test)]
    tree_read_gate_proceed: tokio::sync::Notify,
}

impl AgentSessionRuntime {
    /// Construct the runtime from an initial session + services + factory and
    /// link it to its session.
    ///
    /// Returns an [`Arc`] whose session already carries the runtime's weak
    /// handle. Callers cannot forget to [`link`](Self::link) — the link is
    /// established before the `Arc` leaves this constructor.
    #[must_use]
    pub fn new(
        session: Arc<AgentSession>,
        services: AgentSessionRuntimeServices,
        factory: Arc<dyn CreateAgentSessionRuntimeFactory>,
        diagnostics: Vec<crate::core::agent_session_services::AgentSessionRuntimeDiagnostic>,
        model_fallback_message: Option<String>,
    ) -> Arc<Self> {
        let runtime = Arc::new(Self {
            session: RwLock::new(session),
            services: RwLock::new(services),
            factory,
            remove_file: RwLock::new(Arc::new(|path: &Path| fs::remove_file(path))),
            diagnostics: Arc::new(RwLock::new(diagnostics)),
            model_fallback_message: RwLock::new(model_fallback_message),
            rebind_session: RwLock::new(None),
            before_session_invalidate: RwLock::new(None),
            before_session_replacement: RwLock::new(None),
            self_handle: StdMutex::new(None),
            replacement_lock: Arc::new(AsyncMutex::new(())),
            lifecycle_gate: AsyncRwLock::new(()),
            #[cfg(test)]
            import_commit_gate: RwLock::new(None),
            #[cfg(test)]
            import_commit_started: tokio::sync::Notify::new(),
            #[cfg(test)]
            import_commit_finished: tokio::sync::Notify::new(),
            #[cfg(test)]
            tree_read_gate_acquired: tokio::sync::Notify::new(),
            #[cfg(test)]
            tree_read_gate_proceed: tokio::sync::Notify::new(),
        });
        runtime.link();
        runtime
    }

    /// Snapshot of the current session (cheap Arc clone).
    #[must_use]
    pub fn session(&self) -> Arc<AgentSession> {
        self.read_session()
    }

    /// Link this runtime to its current session and all future replacements.
    fn link(self: &Arc<Self>) {
        let handle = Arc::downgrade(self);
        *self
            .self_handle
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(handle.clone());
        self.read_session().set_runtime_handle(handle);
    }

    /// Replacement mutex used to make prepare plus facade-slot install atomic.
    pub(crate) fn replacement_lock(&self) -> Arc<AsyncMutex<()>> {
        Arc::clone(&self.replacement_lock)
    }

    pub(crate) fn lifecycle_gate(&self) -> &AsyncRwLock<()> {
        &self.lifecycle_gate
    }

    /// Test hook: wait for `navigate_tree` to acquire the lifecycle read gate.
    #[cfg(test)]
    pub(crate) async fn wait_for_tree_read_gate_acquired(&self) {
        self.tree_read_gate_acquired.notified().await;
    }

    /// Test hook: signal that `navigate_tree` has acquired the lifecycle read gate.
    #[cfg(test)]
    pub(crate) fn notify_tree_read_gate_acquired(&self) {
        self.tree_read_gate_acquired.notify_one();
    }

    /// Test hook: wait for the test to signal proceed past the read-gate hook.
    #[cfg(test)]
    pub(crate) async fn wait_for_tree_read_gate_proceed(&self) {
        self.tree_read_gate_proceed.notified().await;
    }

    /// Test hook: signal `navigate_tree` to proceed past the read-gate hook.
    #[cfg(test)]
    pub(crate) fn notify_tree_read_gate_proceed(&self) {
        self.tree_read_gate_proceed.notify_one();
    }

    /// Reject operations while the attached facade owns a pending replacement.
    pub(crate) fn check_no_pending(&self) -> Result<(), AgentSessionRuntimeError> {
        let busy = self
            .read_session()
            .host_extension_runner()
            .is_some_and(|host| host.is_pending_busy());
        if busy {
            Err(AgentSessionRuntimeError::ReplacementBusy)
        } else {
            Ok(())
        }
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
    /// Set the callback invoked only for bridge replacement finalization.
    pub fn set_before_session_replacement(
        &self,
        callback: Option<BeforeSessionReplacementCallback>,
    ) {
        if let Ok(mut g) = self.before_session_replacement.write() {
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
        match self.prepare_switch_session(session_path, options).await? {
            PrepareReplacementOutcome::Cancelled => Ok(SwitchOutcome { cancelled: true }),
            PrepareReplacementOutcome::Prepared(prepared) => {
                self.finalize_replacement_locked(prepared).await;
                Ok(SwitchOutcome { cancelled: false })
            }
        }
    }

    /// Prepare a session switch without tearing down the current session.
    ///
    /// The caller must hold [`Self::replacement_lock`] until it installs the
    /// prepared operation in the facade's pending slot.
    pub(crate) async fn prepare_switch_session(
        &self,
        session_path: &str,
        options: SwitchSessionOptions,
    ) -> Result<PrepareReplacementOutcome, AgentSessionRuntimeError> {
        self.check_no_pending()?;
        let before = self
            .emit_before_switch(SessionStartReason::Resume, Some(session_path))
            .await;
        if before.cancelled {
            return Ok(PrepareReplacementOutcome::Cancelled);
        }

        let extension_flag_values = self.snapshot_extension_flag_values()?;
        let previous_session_file = self.session_file_for_teardown().await;
        let session_manager =
            SessionManager::open(session_path, None, options.cwd_override.as_deref())?;
        self.assert_cwd(&session_manager)?;
        let new_cwd = session_manager.get_cwd().to_owned();
        let target_session_file = session_manager.get_session_file().map(str::to_owned);
        let result = self
            .factory
            .create(CreateAgentSessionRuntimeOptions {
                cwd: new_cwd,
                agent_dir: self.agent_dir(),
                session_manager,
                model: None,
                start_reason: SessionStartReason::Resume,
                previous_session_file,
                extension_flag_values,
            })
            .await?;
        Ok(PrepareReplacementOutcome::Prepared(PreparedReplacement {
            result: Some(result),
            reason: SessionShutdownReason::Resume,
            target_session_file,
        }))
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
        match self.prepare_new_session(options).await? {
            PrepareReplacementOutcome::Cancelled => Ok(SwitchOutcome { cancelled: true }),
            PrepareReplacementOutcome::Prepared(prepared) => {
                self.finalize_replacement_locked(prepared).await;
                Ok(SwitchOutcome { cancelled: false })
            }
        }
    }

    /// Prepare a new session without tearing down the current session.
    ///
    /// The caller must hold [`Self::replacement_lock`] until it installs the
    /// prepared operation in the facade's pending slot.
    pub(crate) async fn prepare_new_session(
        &self,
        options: NewSessionOptions,
    ) -> Result<PrepareReplacementOutcome, AgentSessionRuntimeError> {
        self.check_no_pending()?;
        let before = self.emit_before_switch(SessionStartReason::New, None).await;
        if before.cancelled {
            return Ok(PrepareReplacementOutcome::Cancelled);
        }

        let extension_flag_values = self.snapshot_extension_flag_values()?;
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
                cwd,
                agent_dir: self.agent_dir(),
                session_manager,
                model: Some(self.read_session().model()),
                start_reason: SessionStartReason::New,
                previous_session_file,
                extension_flag_values,
            })
            .await?;
        Ok(PrepareReplacementOutcome::Prepared(PreparedReplacement {
            result: Some(result),
            reason: SessionShutdownReason::New,
            target_session_file,
        }))
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
        let (outcome, selected_text) = self.prepare_fork(entry_id, position).await?;
        match outcome {
            PrepareReplacementOutcome::Cancelled => Ok(ForkOutcome {
                cancelled: true,
                selected_text: None,
            }),
            PrepareReplacementOutcome::Prepared(prepared) => {
                self.finalize_replacement_locked(prepared).await;
                Ok(ForkOutcome {
                    cancelled: false,
                    selected_text,
                })
            }
        }
    }

    /// Prepare a fork without tearing down the current session.
    ///
    /// The caller must hold [`Self::replacement_lock`] until it installs the
    /// prepared operation in the facade's pending slot.
    pub(crate) async fn prepare_fork(
        &self,
        entry_id: &str,
        position: ForkPosition,
    ) -> Result<(PrepareReplacementOutcome, Option<String>), AgentSessionRuntimeError> {
        self.check_no_pending()?;
        let before = self.emit_before_fork(entry_id, position).await;
        if before.cancelled {
            return Ok((PrepareReplacementOutcome::Cancelled, None));
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

        let extension_flag_values = self.snapshot_extension_flag_values()?;
        let previous_session_file = self.session_file_for_teardown().await;
        let cwd = self.cwd();
        let agent_dir = self.agent_dir();
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
            let opts = match target_leaf_id.as_deref() {
                Some(_) => None,
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
                model: Some(self.read_session().model()),
                start_reason: SessionStartReason::Fork,
                previous_session_file,
                extension_flag_values,
            })
            .await?;
        Ok((
            PrepareReplacementOutcome::Prepared(PreparedReplacement {
                result: Some(result),
                reason: SessionShutdownReason::Fork,
                target_session_file,
            }),
            selected_text,
        ))
    }

    /// Finalize a prepared replacement after its host command reports ready.
    pub(crate) async fn finalize_replacement(&self, prepared: PreparedReplacement) {
        let _guard = self.replacement_lock.lock().await;
        if let Ok(callback) = self.before_session_replacement.read()
            && let Some(callback) = callback.clone()
        {
            callback();
        }
        self.finalize_replacement_locked(prepared).await;
    }

    async fn finalize_replacement_locked(&self, mut prepared: PreparedReplacement) {
        let Some(result) = prepared.result.take() else {
            return;
        };
        let reason = prepared.reason;
        let target_session_file = prepared.target_session_file.take();
        let lifecycle_guard = self
            .teardown_current(reason, target_session_file.as_deref())
            .await;
        self.apply(result);
        drop(lifecycle_guard);
        self.finish_session_replacement(None).await;
    }

    /// Dispose a prepared target without changing the current session.
    pub(crate) async fn abort_prepared_replacement(&self, mut prepared: PreparedReplacement) {
        if let Some(result) = prepared.result.take() {
            result.session.dispose().await;
        }
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
        self.check_no_pending()?;
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
            let lifecycle_guard = self
                .teardown_current(SessionShutdownReason::Resume, Some(&paths.destination_text))
                .await;
            self.apply(result);
            drop(lifecycle_guard);
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
            .rebind_session_file_after_atomic_move(&destination_text);
        let lifecycle_guard = self
            .teardown_current(SessionShutdownReason::Resume, Some(&destination_text))
            .await;
        self.apply(result);
        drop(lifecycle_guard);
        self.finish_session_replacement(None).await;
        #[cfg(test)]
        self.import_commit_finished.notify_one();
        SwitchOutcome { cancelled: false }
    }

    /// Dispose the current session and the runtime.
    ///
    /// Serializes against replacements, lets `session_shutdown` handlers finish,
    /// then holds the lifecycle write gate through final tree persistence.
    pub async fn dispose(&self) {
        let _guard = self.replacement_lock.lock().await;
        let _lifecycle_guard = self
            .teardown_current(SessionShutdownReason::Quit, None)
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

    /// Snapshot live extension flag values from the current session's runner.
    ///
    /// Only JSON `Bool` and `String` values are converted to
    /// [`ExtensionFlagValue`]; `Null`, `Number`, `Array`, and `Object` are
    /// rejected with [`AgentSessionRuntimeError::Factory`] so the caller
    /// aborts before tearing down the current session.
    fn snapshot_extension_flag_values(
        &self,
    ) -> Result<BTreeMap<String, ExtensionFlagValue>, AgentSessionRuntimeError> {
        let live = self.read_session().hooks().runner().get_flag_values();
        let mut snapshot = BTreeMap::new();
        for (name, value) in live {
            let converted = match value {
                serde_json::Value::Bool(b) => ExtensionFlagValue::Bool(b),
                serde_json::Value::String(s) => ExtensionFlagValue::Str(s),
                other => {
                    return Err(AgentSessionRuntimeError::Factory(format!(
                        "extension flag \"--{name}\" has unsupported live value type: {other}"
                    )));
                }
            };
            snapshot.insert(name, converted);
        }
        Ok(snapshot)
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
    /// lifecycle exclusion → pre-invalidate → dispose.
    ///
    /// The emit self-gates on `session_shutdown` handler presence and host
    /// errors are isolated. It runs before lifecycle exclusion because hooks
    /// can navigate the session tree. The returned guard keeps final tree
    /// persistence excluded through the caller's apply step; callers must release
    /// it before asynchronous rebind callbacks, which may reacquire the gate. Host
    /// process reap is always performed exactly once inside
    /// [`AgentSession::dispose`] when a concrete host is present, even without
    /// `session_shutdown` handlers.
    async fn teardown_current(
        &self,
        reason: SessionShutdownReason,
        target_session_file: Option<&str>,
    ) -> tokio::sync::RwLockWriteGuard<'_, ()> {
        let session = self.read_session();
        let runner = session.extension_runner();
        let _ = runner
            .emit(AgentSessionEvent::SessionShutdown {
                reason,
                target_session_file: target_session_file.map(str::to_owned),
            })
            .await;
        let lifecycle_guard = self.lifecycle_gate.write().await;
        // Pre-invalidate callbacks reset mode-owned UI during replacement.
        // The extension shutdown handler is a process-exit request that must
        // run for final disposal regardless of whether a UI callback is set.
        let runtime_cb = self
            .before_session_invalidate
            .read()
            .ok()
            .and_then(|g| g.clone());
        if let Some(cb) = runtime_cb {
            cb();
        }
        if reason == SessionShutdownReason::Quit {
            session.invoke_extension_shutdown_handler();
        }
        session.dispose_lifecycle_gate_held().await;
        lifecycle_guard
    }

    /// Apply the factory result: swap session + services + diagnostics.
    fn apply(&self, result: CreateAgentSessionRuntimeResult) {
        let runtime_handle = self
            .self_handle
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone();
        if let Some(handle) = runtime_handle {
            result.session.set_runtime_handle(handle);
        }
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
        let extension_flag_values = self.snapshot_extension_flag_values()?;
        let session_manager = SessionManager::open(session_file, Some(session_dir), cwd_override)?;
        self.assert_cwd(&session_manager)?;
        let new_cwd = session_manager.get_cwd().to_owned();
        self.factory
            .create(CreateAgentSessionRuntimeOptions {
                cwd: new_cwd.clone(),
                agent_dir: self.agent_dir(),
                session_manager,
                model: None,
                start_reason: SessionStartReason::Resume,
                previous_session_file: self.session_file_for_teardown().await,
                extension_flag_values,
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
            Err(move_error) => Err(io::Error::new(
                move_error.kind(),
                format!("link failed ({link_error}); atomic move failed ({move_error})"),
            )),
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

#[cfg(windows)]
fn atomic_move_noreplace(_staged_path: &Path, _destination: &Path) -> io::Result<()> {
    // Windows has no safe, atomic no-replace rename. `std::fs::rename` uses
    // `MoveFileExW` with `MOVEFILE_REPLACE_EXISTING`, so a metadata-check +
    // rename is a TOCTOU race that can silently overwrite a concurrent
    // destination. `windows-sys::MoveFileExW` without that flag would be
    // atomic but is `unsafe`, and the workspace lint forbids `unsafe_code`.
    // `renamore` is unmaintained (>12 months). Fail closed: data integrity
    // wins over a non-atomic fallback. The caller still tries `hard_link`
    // first; only when that fails does this `Unsupported` surface.
    Err(io::Error::new(
        io::ErrorKind::Unsupported,
        "atomic no-replace rename is unavailable on Windows; hard-link fallback also failed",
    ))
}

#[cfg(not(any(
    target_os = "linux",
    target_os = "android",
    target_vendor = "apple",
    target_os = "redox",
    windows
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
) -> Result<Arc<AgentSessionRuntime>, AgentSessionRuntimeError> {
    let result = factory
        .create(CreateAgentSessionRuntimeOptions {
            cwd,
            agent_dir,
            session_manager,
            model: None,
            start_reason: SessionStartReason::Startup,
            previous_session_file: None,
            extension_flag_values: BTreeMap::new(),
        })
        .await?;
    let runtime = AgentSessionRuntime::new(
        result.session,
        result.services,
        factory,
        result.diagnostics,
        result.model_fallback_message,
    );
    Ok(runtime)
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
