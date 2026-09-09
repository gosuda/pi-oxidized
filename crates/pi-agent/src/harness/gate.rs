//! Synchronous admission and cancellation boundary for one harness drive.
//!
//! The gate owns the only lifecycle state for a drive.  Admission is checked
//! while holding the state lock; that check is the linearization point.  A
//! later close or abort may cancel an already-admitted closure through the
//! shared token, but cannot retroactively turn it into a rejected admission.
//! Work that the closure starts must keep using the gate token until that work
//! settles; this type deliberately does not detach or spawn a replacement task.

use std::sync::{Arc, Mutex, MutexGuard};

use futures::future::{BoxFuture, Shared};
use tokio_util::sync::CancellationToken;

use super::result::HarnessFault;

/// The cancellation notification associated with an aborting drive.
///
/// The owner supplies this shared future when it transitions a gate to the
/// aborting state.  Sharing is important because more than one admitted effect
/// can wait for the same owner cancellation without creating timer tasks.
pub type SharedCancellation = Shared<BoxFuture<'static, ()>>;

/// Expected internal control flow when cancellation wins effect admission.
#[derive(Clone, Debug, thiserror::Error)]
#[error("abort requested")]
pub struct AbortRequested {
    cancellation: SharedCancellation,
}

impl AbortRequested {
    /// Waits for the owner-provided cancellation notification.
    pub async fn wait(&self) {
        self.cancellation.clone().await;
    }
}

#[derive(Debug)]
enum GateState {
    Open,
    Aborting { cancellation: SharedCancellation },
    Closed { error: Arc<HarnessFault> },
}

struct GateShared {
    state: Mutex<GateState>,
    token: CancellationToken,
}

impl GateShared {
    fn lock_state(&self) -> MutexGuard<'_, GateState> {
        // Preserve the last coherent lifecycle state if a caller panicked
        // while holding the state lock.
        match self.state.lock() {
            Ok(guard) => guard,
            Err(poisoned) => poisoned.into_inner(),
        }
    }
}

/// Procedure-facing admission capability for one drive pass.
#[derive(Clone)]
pub struct Gate {
    shared: Arc<GateShared>,
}

impl Gate {
    /// Cancellation token handed to admitted effects.
    #[must_use]
    pub fn token(&self) -> &CancellationToken {
        &self.shared.token
    }

    /// Check-then-invoke at the synchronous effect boundary.
    ///
    /// The lifecycle lock protects the state check only.  An open check is the
    /// linearization point for admission; the closure is then invoked without
    /// holding a non-reentrant mutex.  If close wins a later state check it
    /// cancels this already-admitted operation through the retained token, but
    /// it cannot turn that operation into a new admission.
    ///
    /// # Errors
    ///
    /// Returns [`GateRejection::Aborted`] while the gate is aborting and
    /// [`GateRejection::Closed`] after the drive has closed.
    pub fn admit<T>(&self, invoke: impl FnOnce() -> T) -> Result<T, GateRejection> {
        let rejection = {
            let state = self.shared.lock_state();
            match &*state {
                GateState::Open => None,
                GateState::Aborting { cancellation } => {
                    Some(GateRejection::Aborted(AbortRequested {
                        cancellation: cancellation.clone(),
                    }))
                }
                GateState::Closed { error } => Some(GateRejection::Closed(Arc::clone(error))),
            }
        };
        match rejection {
            None => Ok(invoke()),
            Some(rejection) => Err(rejection),
        }
    }
}

impl std::fmt::Debug for Gate {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.debug_struct("Gate").finish_non_exhaustive()
    }
}

/// Owner-facing lifecycle controls for one drive pass.
#[derive(Clone)]
pub struct GateControl {
    shared: Arc<GateShared>,
}

impl GateControl {
    /// Transitions `Open` to `Aborting`.
    ///
    /// Repeated calls, including calls after close, are intentionally ignored;
    /// the first lifecycle decision owns the cancellation future.
    pub fn begin_abort(&self, cancellation: SharedCancellation) {
        let mut state = self.shared.lock_state();
        if matches!(&*state, GateState::Open) {
            *state = GateState::Aborting { cancellation };
        }
    }

    /// Cancels the gate token when the gate is aborting.
    pub fn signal_abort(&self) {
        let state = self.shared.lock_state();
        let should_cancel = matches!(&*state, GateState::Aborting { .. });
        drop(state);
        if should_cancel {
            self.shared.token.cancel();
        }
    }

    /// Transitions the gate to its absorbing closed state and cancels effects.
    ///
    /// Closing is idempotent.  The token is cancelled after publishing the
    /// closed state so a concurrently admitted effect cannot observe an open
    /// gate after closure begins.
    pub fn close(&self, error: Arc<HarnessFault>) {
        let mut state = self.shared.lock_state();
        if matches!(&*state, GateState::Closed { .. }) {
            return;
        }
        *state = GateState::Closed { error };
        drop(state);
        self.shared.token.cancel();
    }
}

impl std::fmt::Debug for GateControl {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("GateControl")
            .finish_non_exhaustive()
    }
}

/// Creates an open gate and its owner controls.
#[must_use]
pub fn create_gate() -> (Gate, GateControl) {
    let shared = Arc::new(GateShared {
        state: Mutex::new(GateState::Open),
        token: CancellationToken::new(),
    });
    (
        Gate {
            shared: Arc::clone(&shared),
        },
        GateControl { shared },
    )
}

/// Result of attempting to admit work at the gate.
#[derive(Clone, Debug, thiserror::Error)]
pub enum GateRejection {
    /// The owner requested abort before this effect was admitted.
    #[error(transparent)]
    Aborted(#[from] AbortRequested),
    /// The drive was closed with an infrastructure fault.
    #[error(transparent)]
    Closed(Arc<HarnessFault>),
}

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(Debug, thiserror::Error)]
    #[error("test gate closure")]
    struct TestFault;

    fn fault() -> Arc<HarnessFault> {
        Arc::new(HarnessFault {
            message: "closed for test".to_owned(),
            cause: Box::new(TestFault),
        })
    }

    #[test]
    fn admission_callback_may_close_without_reentrant_deadlock() {
        let (gate, control) = create_gate();
        let result = gate.admit(|| {
            control.close(fault());
            7_u8
        });

        assert!(matches!(result, Ok(7)));
        assert!(matches!(gate.admit(|| 8_u8), Err(GateRejection::Closed(_))));
    }

    #[test]
    fn abort_is_distinct_from_close_and_cancels_token_only_when_signalled() {
        use futures::FutureExt;

        let (gate, control) = create_gate();
        let cancellation = futures::future::ready(()).boxed().shared();
        control.begin_abort(cancellation);

        assert!(!gate.token().is_cancelled());
        assert!(matches!(gate.admit(|| ()), Err(GateRejection::Aborted(_))));

        control.signal_abort();
        assert!(gate.token().is_cancelled());
    }
}
