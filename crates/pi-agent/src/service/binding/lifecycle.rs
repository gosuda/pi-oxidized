use std::future::Future;
use std::sync::{Arc, Mutex, MutexGuard};

use tokio::sync::Notify;
use tokio_util::sync::CancellationToken;

use crate::context::Context;

use super::super::error::ServiceError;
pub(crate) type ErrorReporter = super::BindingErrorReporter;
pub(crate) type AccessChecker = super::BindingAccessChecker;

/// Shared binding lifetime and call cancellation state.
pub(crate) struct Lifecycle {
    bound: std::sync::atomic::AtomicBool,
    disposed: std::sync::atomic::AtomicBool,
    pub(crate) calls: Arc<CallTracker>,
    pub(crate) report_error: ErrorReporter,
    pub(crate) assert_access: AccessChecker,
}

impl Lifecycle {
    pub(crate) fn new(
        bound: bool,
        report_error: ErrorReporter,
        assert_access: AccessChecker,
    ) -> Arc<Self> {
        Arc::new(Self {
            bound: std::sync::atomic::AtomicBool::new(bound),
            disposed: std::sync::atomic::AtomicBool::new(false),
            calls: Arc::new(CallTracker::new()),
            report_error,
            assert_access,
        })
    }

    pub(crate) fn is_bound(&self) -> bool {
        self.bound.load(std::sync::atomic::Ordering::Acquire)
    }

    pub(crate) fn is_disposed(&self) -> bool {
        self.disposed.load(std::sync::atomic::Ordering::Acquire)
    }

    pub(crate) fn set_bound(&self, bound: bool) {
        self.bound
            .store(bound, std::sync::atomic::Ordering::Release);
    }

    pub(crate) fn dispose(&self) {
        self.disposed
            .store(true, std::sync::atomic::Ordering::Release);
        self.set_bound(false);
        self.calls.close();
    }

    pub(crate) fn assert_access(&self) -> Result<(), ServiceError> {
        if self.is_disposed() {
            return Err(ServiceError::disposed("Remote service binding is disposed"));
        }
        (self.assert_access)()
    }

    pub(crate) fn report(&self, error: ServiceError) {
        (self.report_error)(error);
    }
}

struct CallState {
    active: usize,
    closed: bool,
}

/// Tracks invocation futures so disposal can cancel and drain them.
pub(crate) struct CallTracker {
    state: Mutex<CallState>,
    cancel: CancellationToken,
    done: Notify,
}

impl CallTracker {
    fn new() -> Self {
        Self {
            state: Mutex::new(CallState {
                active: 0,
                closed: false,
            }),
            cancel: CancellationToken::new(),
            done: Notify::new(),
        }
    }

    pub(crate) fn begin(self: &Arc<Self>) -> Result<CallPermit, ServiceError> {
        let mut state = lock(&self.state);
        if state.closed {
            return Err(ServiceError::disposed("Remote service binding is disposed"));
        }
        state.active = state.active.saturating_add(1);
        Ok(CallPermit {
            tracker: Arc::clone(self),
        })
    }

    fn finish(&self) {
        let mut state = lock(&self.state);
        state.active = state.active.saturating_sub(1);
        if state.active == 0 {
            self.done.notify_waiters();
        }
    }

    pub(crate) fn close(&self) {
        let mut state = lock(&self.state);
        state.closed = true;
        drop(state);
        self.cancel.cancel();
        self.done.notify_waiters();
    }

    pub(crate) async fn drain(&self) {
        loop {
            let notified = self.done.notified();
            if lock(&self.state).active == 0 {
                return;
            }
            notified.await;
        }
    }

    pub(crate) fn cancellation(&self) -> CancellationToken {
        self.cancel.clone()
    }
}

pub(crate) struct CallPermit {
    tracker: Arc<CallTracker>,
}

impl Drop for CallPermit {
    fn drop(&mut self) {
        self.tracker.finish();
    }
}

pub(crate) async fn await_with_lifetime<T>(
    context: &Context,
    lifetime: CancellationToken,
    future: impl Future<Output = Result<T, ServiceError>>,
) -> Result<T, ServiceError> {
    tokio::pin!(future);
    let caller_cancellation = context.token().cloned();
    tokio::select! {
        result = &mut future => result,
        () = lifetime.cancelled() => Err(ServiceError::Cancelled),
        () = async {
            if let Some(token) = caller_cancellation {
                token.cancelled().await;
            } else {
                std::future::pending::<()>().await;
            }
        } => Err(ServiceError::Cancelled),
    }
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    match mutex.lock() {
        Ok(guard) => guard,
        Err(poisoned) => poisoned.into_inner(),
    }
}
