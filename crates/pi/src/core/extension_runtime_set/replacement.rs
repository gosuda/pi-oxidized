//! Ready-gated replacement: the sole owner of the pending/finalizing slot and
//! of the bridge tokens that correlate host readiness with one operation.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex as StdMutex, Weak};

use tokio::sync::oneshot;

use super::EndpointId;
use super::PreparedReload;
use crate::core::agent_session::AgentSession;
use crate::core::agent_session::events::SessionShutdownReason;
use crate::core::agent_session_runtime::{CreateAgentSessionRuntimeResult, spawn_runtime_safe};
use crate::core::model_runtime::ModelRuntime;

/// Prepared resources owned by the facade while a replacement waits for host readiness.
pub(crate) enum PendingReadyOp {
    /// A fully-created session runtime waiting to replace the current runtime.
    Replacement {
        /// New runtime state. It is neither applied nor disposed before the ready decision.
        result: CreateAgentSessionRuntimeResult,
        /// Shutdown reason emitted by the old session during finalization.
        reason: SessionShutdownReason,
        /// Session file that will replace the old session file, when known.
        target_session_file: Option<String>,
    },
    /// A prepared extension generation waiting to replace the published generation.
    Reload {
        /// Unpublished extension generation.
        prepared: PreparedReload,
        /// Provider registry receiving the committed generation.
        model_runtime: Arc<ModelRuntime>,
    },
}

impl PendingReadyOp {
    pub(super) fn replacement_target(&self) -> Option<Arc<AgentSession>> {
        match self {
            Self::Replacement { result, .. } => Some(Arc::clone(&result.session)),
            Self::Reload { .. } => None,
        }
    }
}

pub(super) enum PendingReadyState {
    None,
    Pending {
        op: PendingReadyOp,
        token: String,
        ready_tx: oneshot::Sender<()>,
        owner: Option<EndpointId>,
    },
    Finalizing {
        op: Option<PendingReadyOp>,
        token: String,
        owner: Option<EndpointId>,
        replacement_target: Option<Arc<AgentSession>>,
    },
}

/// Releases the finalizing slot when the finalizer completes or unwinds.
///
/// Returned from [`ReplacementGate::take_finalizing`] alongside the transferred
/// operation. If the finalizer task panics or returns early without calling
/// [`ReplacementGate::finish_finalize`], this guard's `Drop` impl clears the
/// slot so future replacements are not wedged.
pub(crate) struct FinalizeGuard {
    gate: Weak<ReplacementGate>,
    token: String,
}

impl Drop for FinalizeGuard {
    fn drop(&mut self) {
        if let Some(gate) = self.gate.upgrade() {
            // Idempotent: a normal finish already cleared the slot.
            let _ = gate.finish_finalize(&self.token);
        }
    }
}

/// Owns the one ready-gated replacement slot and mints every token that
/// addresses it, so token minting and matching never leave this module.
pub(super) struct ReplacementGate {
    inner: Arc<StdMutex<PendingReadyState>>,
    next_token: AtomicU64,
}

impl ReplacementGate {
    pub(super) fn new() -> Self {
        Self {
            inner: Arc::new(StdMutex::new(PendingReadyState::None)),
            next_token: AtomicU64::new(1),
        }
    }

    /// Weak handle to the slot for relay drop callbacks.
    pub(super) fn weak(&self) -> Weak<StdMutex<PendingReadyState>> {
        Arc::downgrade(&self.inner)
    }

    /// Borrow the slot. Callers outside the gate must not re-enter it while
    /// the guard is alive.
    pub(super) fn state(&self) -> std::sync::MutexGuard<'_, PendingReadyState> {
        self.inner
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// The candidate's target session when `token` and `origin` match a
    /// live pending or finalizing replacement. Bridge-token matching lives
    /// only inside the gate.
    pub(super) fn candidate_target_for(
        &self,
        token: &str,
        origin: Option<EndpointId>,
    ) -> Option<Arc<AgentSession>> {
        match &*self.state() {
            PendingReadyState::Pending {
                op,
                token: pending,
                owner,
                ..
            } if pending == token && *owner == origin => op.replacement_target(),
            PendingReadyState::Finalizing {
                token: pending,
                owner,
                replacement_target,
                ..
            } if pending == token && *owner == origin => replacement_target.clone(),
            _ => None,
        }
    }

    /// Whether an operation-scoped event may run now: a pending
    /// replacement matching both `token` and `origin` exists.
    pub(super) fn operation_accepts(&self, token: &str, origin: Option<EndpointId>) -> bool {
        matches!(
            &*self.state(),
            PendingReadyState::Pending {
                token: pending,
                owner,
                ..
            } if pending == token && *owner == origin
        )
    }

    /// Commit gate: clear the finalizing slot when `token` matches and the
    /// routed session is the replacement target. A mismatch clears nothing.
    pub(super) fn finalize_commit_target(&self, token: &str, routed: &Arc<AgentSession>) -> bool {
        let mut state = self.state();
        match &*state {
            PendingReadyState::Finalizing {
                op: None,
                token: pending,
                replacement_target: Some(target),
                ..
            } if pending == token && Arc::ptr_eq(target, routed) => {
                *state = PendingReadyState::None;
                true
            }
            _ => false,
        }
    }

    /// The replacement target owning `token`, regardless of origin — the
    /// setup-entries scope check.
    pub(super) fn setup_target_for(&self, token: &str) -> Option<Arc<AgentSession>> {
        match &*self.state() {
            PendingReadyState::Pending {
                op, token: pending, ..
            } if pending == token => op.replacement_target(),
            PendingReadyState::Finalizing {
                token: pending,
                replacement_target,
                ..
            } if pending == token => replacement_target.clone(),
            _ => None,
        }
    }

    /// Mint a facade-scoped replacement token safe to serialize across JavaScript.
    #[must_use]
    #[allow(clippy::expect_used)]
    pub(crate) fn next_replacement_token(&self) -> String {
        self.next_token
            .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |current| {
                current.checked_add(1)
            })
            .expect("replacement token space exhausted")
            .to_string()
    }

    /// Install the sole ready-gated operation before its success response is written.
    ///
    /// On conflict, ownership of `op` is returned to the caller for explicit cleanup.
    #[allow(clippy::result_large_err)]
    pub(crate) fn install_pending(
        &self,
        token: String,
        op: PendingReadyOp,
    ) -> Result<oneshot::Receiver<()>, PendingReadyOp> {
        let mut state = self.state();
        if !matches!(*state, PendingReadyState::None) {
            return Err(op);
        }
        let (ready_tx, ready_rx) = oneshot::channel();
        *state = PendingReadyState::Pending {
            op,
            token,
            ready_tx,
            owner: None,
        };
        Ok(ready_rx)
    }

    /// Correlate a host-ready event and advance its operation into finalization.
    ///
    /// Returns false for stale/mismatched tokens and for a waiter that was already dropped.
    pub(crate) fn complete_ready(&self, token: &str) -> bool {
        let mut state = self.state();
        let current = std::mem::replace(&mut *state, PendingReadyState::None);
        let (op, pending_token, ready_tx, owner) = match current {
            PendingReadyState::Pending {
                op,
                token,
                ready_tx,
                owner,
            } => (op, token, ready_tx, owner),
            other => {
                *state = other;
                return false;
            }
        };
        if pending_token != token {
            *state = PendingReadyState::Pending {
                op,
                token: pending_token,
                ready_tx,
                owner,
            };
            return false;
        }
        let replacement_target = op.replacement_target();
        *state = PendingReadyState::Finalizing {
            op: Some(op),
            token: pending_token,
            owner,
            replacement_target,
        };
        if ready_tx.send(()).is_ok() {
            return true;
        }
        let drained = match std::mem::replace(&mut *state, PendingReadyState::None) {
            PendingReadyState::Finalizing { op, .. } => op,
            PendingReadyState::None | PendingReadyState::Pending { .. } => None,
        };
        drop(state);
        if let Some(op) = drained {
            discard(op);
        }
        false
    }

    /// Bind the endpoint that owns a pending token, so only its bridges may
    /// drive the operation forward.
    pub(super) fn bind_owner(&self, token: &str, owner: EndpointId) {
        let mut state = self.state();
        if let PendingReadyState::Pending {
            token: pending_token,
            owner: pending_owner,
            ..
        } = &mut *state
            && pending_token == token
        {
            *pending_owner = Some(owner);
        }
    }

    /// Transfer a token-correlated operation to its finalizer while retaining slot ownership.
    ///
    /// Returns the operation and a [`FinalizeGuard`] that releases the
    /// finalizing slot on drop. If the finalizer completes normally it should
    /// call [`ReplacementGate::finish_finalize`] to clear the slot explicitly;
    /// if it panics or returns early, the guard's `Drop` impl clears the slot
    /// so future replacements are not wedged.
    pub(crate) fn take_finalizing(
        self: &Arc<Self>,
        token: &str,
    ) -> Option<(PendingReadyOp, FinalizeGuard)> {
        let mut state = self.state();
        match &mut *state {
            PendingReadyState::Finalizing {
                op,
                token: pending_token,
                ..
            } if pending_token == token => op.take().map(|op| {
                let guard = FinalizeGuard {
                    gate: Arc::downgrade(self),
                    token: token.to_owned(),
                };
                (op, guard)
            }),
            PendingReadyState::None
            | PendingReadyState::Pending { .. }
            | PendingReadyState::Finalizing { .. } => None,
        }
    }

    /// Release the finalizing slot after the transferred operation was applied.
    ///
    /// Tolerates an already-cleared slot: if the slot was cleared by a
    /// [`FinalizeGuard`] drop or a prior call, this returns `true` so callers
    /// that race a guard drop with an explicit finish do not observe a
    /// spurious failure.
    pub(crate) fn finish_finalize(&self, token: &str) -> bool {
        let mut state = self.state();
        match &*state {
            PendingReadyState::Finalizing {
                op: None,
                token: pending_token,
                ..
            } if pending_token == token => {
                *state = PendingReadyState::None;
                true
            }
            // Slot already cleared (e.g. by a guard drop racing this call).
            PendingReadyState::None => true,
            _ => false,
        }
    }

    /// Abort an operation that has not been transferred to a finalizer.
    #[must_use]
    pub(crate) fn abort_pending(&self, token: &str) -> Option<PendingReadyOp> {
        let mut state = self.state();
        let matches = match &*state {
            PendingReadyState::Pending {
                token: pending_token,
                ..
            }
            | PendingReadyState::Finalizing {
                token: pending_token,
                op: Some(_),
                ..
            } => pending_token == token,
            PendingReadyState::None | PendingReadyState::Finalizing { op: None, .. } => false,
        };
        if !matches {
            return None;
        }
        match std::mem::replace(&mut *state, PendingReadyState::None) {
            PendingReadyState::Pending { op, .. } => Some(op),
            PendingReadyState::Finalizing { op, .. } => op,
            PendingReadyState::None => None,
        }
    }

    /// Abort an exact token that is still waiting for host readiness.
    ///
    /// Accepted readiness is irreversible: `Finalizing` states are never removed.
    pub(crate) fn abort_waiting_ready(&self, token: &str, owner: Option<EndpointId>) -> bool {
        let Some(op) = abort_pending_drop(&self.inner, token, owner) else {
            return false;
        };
        discard(op);
        true
    }

    /// Drain any facade-owned prepared resources and wake a pending waiter.
    #[must_use]
    pub(crate) fn drain_pending(&self) -> Option<PendingReadyOp> {
        match std::mem::replace(&mut *self.state(), PendingReadyState::None) {
            PendingReadyState::Pending { op, .. } => Some(op),
            PendingReadyState::Finalizing { op, .. } => op,
            PendingReadyState::None => None,
        }
    }

    /// Drain and dispose any operation the slot still owns.
    pub(crate) fn drain_and_discard(&self) {
        if let Some(op) = self.drain_pending() {
            discard(op);
        }
    }

    /// Whether a ready-gated operation is pending or finalizing.
    #[must_use]
    pub(crate) fn is_pending_busy(&self) -> bool {
        !matches!(*self.state(), PendingReadyState::None)
    }

    /// Test-only entry into the drop-path abort, so set-level tests can assert
    /// token/owner matching without a real relay drop.
    #[cfg(test)]
    pub(super) fn abort_pending_drop_for_test(
        &self,
        token: &str,
        owner: Option<EndpointId>,
    ) -> Option<PendingReadyOp> {
        abort_pending_drop(&self.inner, token, owner)
    }
}

/// Abort a token whose readiness frame was dropped in transit, then dispose it.
pub(super) fn abort_dropped(
    pending: &Weak<StdMutex<PendingReadyState>>,
    token: &str,
    owner: Option<EndpointId>,
) {
    let Some(pending) = pending.upgrade() else {
        return;
    };
    if let Some(op) = abort_pending_drop(&pending, token, owner) {
        discard(op);
    }
}

/// Abort the operation owned by `owner` through a weak slot handle.
pub(super) fn abort_owned_weak(pending: &Weak<StdMutex<PendingReadyState>>, owner: EndpointId) {
    let Some(pending) = pending.upgrade() else {
        return;
    };
    if let Some(op) = abort_owner(&pending, owner) {
        discard(op);
    }
}

fn discard(op: PendingReadyOp) {
    match op {
        PendingReadyOp::Replacement { result, .. } => {
            spawn_runtime_safe("prepared-replacement-discard", async move {
                result.session.dispose().await;
            });
        }
        PendingReadyOp::Reload { .. } => {}
    }
}

/// Drop-specific token removal: removes ONLY a matching `Pending` state, never
/// `Finalizing`. A duplicate dropped readiness frame must not revoke an already
/// accepted `complete_ready` that won the race. Returns the removed operation
/// so the caller can discard it after releasing the mutex guard.
fn abort_pending_drop(
    pending: &StdMutex<PendingReadyState>,
    token: &str,
    owner: Option<EndpointId>,
) -> Option<PendingReadyOp> {
    let mut state = pending
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    match &*state {
        PendingReadyState::Pending {
            token: pending_token,
            owner: pending_owner,
            ..
        } if pending_token == token && *pending_owner == owner => {}
        _ => return None,
    }
    match std::mem::replace(&mut *state, PendingReadyState::None) {
        PendingReadyState::Pending { op, ready_tx, .. } => {
            drop(ready_tx);
            Some(op)
        }
        _ => None,
    }
}

fn abort_owner(pending: &StdMutex<PendingReadyState>, owner: EndpointId) -> Option<PendingReadyOp> {
    let mut state = pending
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    match &*state {
        PendingReadyState::Pending {
            owner: Some(pending_owner),
            ..
        } if *pending_owner == owner => {}
        _ => return None,
    }
    match std::mem::replace(&mut *state, PendingReadyState::None) {
        PendingReadyState::Pending { op, ready_tx, .. } => {
            drop(ready_tx);
            Some(op)
        }
        _ => None,
    }
}
