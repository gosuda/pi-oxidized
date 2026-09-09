//! Demand- and operation-driven session-worker lifetime.
//!
//! This is the native equivalent of `WorkerLifecycle` in
//! `packages/coding-agent/src/experimental/session-worker.ts:190-320`.
//! Demand belongs to a server generation. A disconnected generation keeps its
//! demands alive for the orphan grace; a replacement generation can reconnect
//! and clear those timers. Retirement is allowed only after the initial grace
//! has elapsed, demand has been initialized, all demands have gone, and no
//! operation or acknowledgement hold remains.
//!
//! The timer driver uses Tokio's monotonic [`tokio::time::Instant`]. There is
//! one owned task per lifecycle rather than one detached task per demand. A
//! deadline carries a token, so a wake for an old deadline cannot remove a
//! demand that has since been reattached or reconnected. Retirement callbacks
//! are always invoked after the state lock is released.

use std::collections::{HashMap, HashSet};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, Weak};
use std::time::Duration;

use pi_agent::service::value::JsString;
use thiserror::Error;
use tokio::sync::Notify;
use tokio::task::JoinHandle;
use tokio::time::Instant;

/// Default grace period before a worker with no initialized demand retires.
pub const DEFAULT_INITIAL_DEMAND_GRACE_MS: u64 = 10_000;
/// Default grace period for demands orphaned by a disconnected server.
pub const DEFAULT_ORPHAN_DEMAND_GRACE_MS: u64 = 30_000;
/// Environment variable overriding [`DEFAULT_INITIAL_DEMAND_GRACE_MS`].
pub const SESSION_WORKER_INITIAL_DEMAND_GRACE_ENV: &str =
    "__PI_SESSION_WORKER_INITIAL_DEMAND_GRACE_MS";
/// Environment variable overriding [`DEFAULT_ORPHAN_DEMAND_GRACE_MS`].
pub const SESSION_WORKER_ORPHAN_DEMAND_GRACE_ENV: &str =
    "__PI_SESSION_WORKER_ORPHAN_DEMAND_GRACE_MS";

const MAX_SAFE_INTEGER_MILLIS: u64 = 9_007_199_254_740_991;
const MAX_SAFE_INTEGER_MILLIS_F64: f64 = 9_007_199_254_740_991.0;

/// Failure raised while validating or applying worker lifecycle state.
#[derive(Clone, Debug, Eq, Error, PartialEq)]
pub enum WorkerLifecycleError {
    /// A demand or request arrived after retirement won the race.
    #[error("Session worker is retiring")]
    Retiring,
    /// The lifecycle has been explicitly closed.
    #[error("Session worker is closed")]
    Closed,
    /// A request used a server generation other than the connected one.
    #[error("Session worker received a request from a stale server generation")]
    StaleRequestServerGeneration,
    /// A demand used a server generation other than the connected one.
    #[error("Session worker received demand from a stale server generation")]
    StaleDemandServerGeneration,
    /// A request did not name a currently active attachment.
    #[error("Session worker request does not match the active attachment")]
    InactiveAttachment,
    /// A lifecycle delay was not a non-negative JavaScript safe integer.
    #[error("{name} must be a non-negative safe integer")]
    InvalidDelay {
        /// Name of the delay that was out of range (e.g. an environment variable).
        name: String,
    },
}

/// Retirement callback shared by the lifecycle and its timer driver.
pub type OnRetire = Arc<dyn Fn() + Send + Sync>;

/// Configuration for one worker lifecycle.
pub struct WorkerLifecycleOptions {
    /// Server generation known at worker launch, if one was already active.
    pub initial_server_connection_id: Option<JsString>,
    /// Initial demand grace.
    pub initial_demand_grace: Duration,
    /// Orphan demand grace.
    pub orphan_demand_grace: Duration,
    /// Invoked exactly once when retirement becomes eligible.
    pub on_retire: OnRetire,
}

/// One operation kind tracked by the worker lifetime guard.
///
/// The session domain already owns this closed set; re-exporting it keeps
/// lifecycle events and durable operation records on one type.
pub use pi_agent::session::OperationKind;

/// Identifier input accepted by lifecycle methods.
///
/// Wire-facing callers can pass [`JsString`] without crossing through lossy
/// UTF-8 conversion. Native callers that already own ordinary Rust text can
/// pass `&str` or `String`; those values are encoded as UTF-16 at the state
/// boundary.
pub trait WorkerIdentifier {
    /// Returns the exact Chord string represented by this identifier.
    fn to_js_string(&self) -> JsString;
}

impl WorkerIdentifier for JsString {
    fn to_js_string(&self) -> JsString {
        self.clone()
    }
}

impl WorkerIdentifier for str {
    fn to_js_string(&self) -> JsString {
        JsString::from_utf8(self)
    }
}

impl WorkerIdentifier for String {
    fn to_js_string(&self) -> JsString {
        JsString::from_utf8(self)
    }
}

impl WorkerIdentifier for pi_agent::session::LaneName {
    fn to_js_string(&self) -> JsString {
        JsString::from_utf8(self.as_str())
    }
}

impl WorkerIdentifier for pi_agent::session::OperationId {
    fn to_js_string(&self) -> JsString {
        JsString::from_utf8(self.as_str())
    }
}

/// A release guard returned for one operation or acknowledgement hold.
///
/// Dropping the guard releases it. [`Self::release`] is idempotent so a caller
/// can release explicitly and still safely let the guard drop.
pub struct RetirementHold {
    inner: Weak<Inner>,
    released: AtomicBool,
}

impl RetirementHold {
    /// Releases this hold once and re-runs retirement reconciliation.
    pub fn release(&self) {
        if self.released.swap(true, Ordering::AcqRel) {
            return;
        }
        let Some(inner) = self.inner.upgrade() else {
            return;
        };
        release_hold(&inner);
    }
}

impl Drop for RetirementHold {
    fn drop(&mut self) {
        self.release();
    }
}

/// Worker-local lifecycle state and its owned monotonic timer driver.
pub struct WorkerLifecycle {
    inner: Arc<Inner>,
}

struct Inner {
    state: Mutex<State>,
    wake: Arc<Notify>,
    closed_notify: Arc<Notify>,
    timer: Mutex<Option<JoinHandle<()>>>,
    orphan_demand_grace: Duration,
    on_retire: OnRetire,
}

#[expect(
    clippy::struct_excessive_bools,
    reason = "lifecycle state uses four explicit boolean flags for the retiring/closed/demand/timer predicates"
)]
struct State {
    current_server_connection_id: Option<JsString>,
    demands: HashMap<DemandKey, Demand>,
    active_operations: HashSet<OperationKey>,
    initial_deadline: Option<Deadline>,
    next_timer_token: u64,
    demand_initialized: bool,
    retirement_holds: usize,
    retiring: bool,
    closed: bool,
    timer_finished: bool,
}

struct Demand {
    orphan_deadline: Option<Deadline>,
}

#[derive(Clone, Copy)]
struct Deadline {
    at: Instant,
    token: u64,
}

#[derive(Clone, Debug, Eq, Hash, PartialEq)]
struct DemandKey {
    server_connection_id: JsString,
    attachment_id: JsString,
}

#[derive(Clone, Debug, Eq, Hash, PartialEq)]
struct OperationKey {
    kind: u8,
    lane: JsString,
    operation_id: JsString,
}

impl WorkerLifecycle {
    /// Starts a lifecycle and its one owned monotonic timer task.
    ///
    /// The worker process constructs this from its Tokio runtime. If a caller
    /// creates it outside a runtime, state transitions remain safe and are
    /// reconciled by subsequent lifecycle calls; no task is detached or leaked.
    #[must_use]
    pub fn new(options: WorkerLifecycleOptions) -> Self {
        let WorkerLifecycleOptions {
            initial_server_connection_id,
            initial_demand_grace,
            orphan_demand_grace,
            on_retire,
        } = options;
        let wake = Arc::new(Notify::new());
        let closed_notify = Arc::new(Notify::new());
        let inner = Arc::new(Inner {
            state: Mutex::new(State {
                current_server_connection_id: initial_server_connection_id,
                demands: HashMap::new(),
                active_operations: HashSet::new(),
                initial_deadline: Some(Deadline {
                    at: Instant::now() + initial_demand_grace,
                    token: 1,
                }),
                next_timer_token: 2,
                demand_initialized: false,
                retirement_holds: 0,
                retiring: false,
                closed: false,
                timer_finished: false,
            }),
            wake: Arc::clone(&wake),
            closed_notify,
            timer: Mutex::new(None),
            orphan_demand_grace,
            on_retire,
        });

        let timer = tokio::runtime::Handle::try_current()
            .ok()
            .map(|handle| handle.spawn(timer_loop(Arc::downgrade(&inner), wake)));
        if timer.is_none() {
            lock(&inner.state).timer_finished = true;
        }
        *lock(&inner.timer) = timer;
        Self { inner }
    }

    /// Records that a server generation is connected and cancels orphan
    /// deadlines belonging to that generation.
    pub fn server_connected<S>(&self, server_connection_id: &S)
    where
        S: WorkerIdentifier + ?Sized,
    {
        let server_connection_id = server_connection_id.to_js_string();
        expire_due(&self.inner, None);
        {
            let mut state = lock(&self.inner.state);
            if state.closed {
                return;
            }
            state.current_server_connection_id = Some(server_connection_id.clone());
            for (key, demand) in &mut state.demands {
                if key.server_connection_id == server_connection_id {
                    demand.orphan_deadline = None;
                }
            }
        }
        self.inner.wake.notify_one();
    }

    /// Records that a server generation disconnected and starts orphan grace
    /// for its outstanding demands. Calling this repeatedly does not reset a
    /// deadline already running for the same demand.
    pub fn server_disconnected<S>(&self, server_connection_id: &S)
    where
        S: WorkerIdentifier + ?Sized,
    {
        let server_connection_id = server_connection_id.to_js_string();
        expire_due(&self.inner, None);
        let deadline = Instant::now() + self.inner.orphan_demand_grace;
        {
            let mut state = lock(&self.inner.state);
            if state.closed {
                return;
            }
            if state.current_server_connection_id.as_ref() == Some(&server_connection_id) {
                state.current_server_connection_id = None;
            }
            let mut token = state.next_timer_token;
            for (key, demand) in &mut state.demands {
                if key.server_connection_id != server_connection_id
                    || demand.orphan_deadline.is_some()
                {
                    continue;
                }
                demand.orphan_deadline = Some(Deadline {
                    at: deadline,
                    token,
                });
                token = next_timer_token(token);
            }
            state.next_timer_token = token;
        }
        self.inner.wake.notify_one();
    }

    /// Begins a request for an active attachment and returns its retirement
    /// hold. Stale generations and orphaned or inactive attachments are
    /// rejected before a hold is created.
    ///
    /// # Errors
    /// Returns [`WorkerLifecycleError::Retiring`] or
    /// [`WorkerLifecycleError::Closed`] after a terminal transition,
    /// [`WorkerLifecycleError::StaleRequestServerGeneration`] for a stale
    /// generation, or [`WorkerLifecycleError::InactiveAttachment`] when the
    /// attachment is not currently active.
    pub fn begin_request<S, A>(
        &self,
        server_connection_id: &S,
        attachment_id: &A,
    ) -> Result<RetirementHold, WorkerLifecycleError>
    where
        S: WorkerIdentifier + ?Sized,
        A: WorkerIdentifier + ?Sized,
    {
        let server_connection_id = server_connection_id.to_js_string();
        let attachment_id = attachment_id.to_js_string();
        expire_due(&self.inner, None);
        let mut state = lock(&self.inner.state);
        if state.closed {
            return Err(WorkerLifecycleError::Closed);
        }
        if state.retiring {
            return Err(WorkerLifecycleError::Retiring);
        }
        if state.current_server_connection_id.as_ref() != Some(&server_connection_id) {
            return Err(WorkerLifecycleError::StaleRequestServerGeneration);
        }
        let key = DemandKey {
            server_connection_id,
            attachment_id,
        };
        let Some(demand) = state.demands.get(&key) else {
            return Err(WorkerLifecycleError::InactiveAttachment);
        };
        if demand.orphan_deadline.is_some() {
            return Err(WorkerLifecycleError::InactiveAttachment);
        }
        state.retirement_holds = state.retirement_holds.saturating_add(1);
        drop(state);
        Ok(RetirementHold {
            inner: Arc::downgrade(&self.inner),
            released: AtomicBool::new(false),
        })
    }

    /// Holds retirement while a demand acknowledgement is being applied.
    #[must_use]
    pub fn hold_retirement(&self) -> RetirementHold {
        expire_due(&self.inner, None);
        {
            let mut state = lock(&self.inner.state);
            if !state.closed && !state.retiring {
                state.retirement_holds = state.retirement_holds.saturating_add(1);
            }
        }
        RetirementHold {
            inner: Arc::downgrade(&self.inner),
            released: AtomicBool::new(false),
        }
    }

    /// Applies or removes one attachment demand for the active generation.
    ///
    /// # Errors
    /// Returns [`WorkerLifecycleError::Retiring`] or
    /// [`WorkerLifecycleError::Closed`] after a terminal transition, or
    /// [`WorkerLifecycleError::StaleDemandServerGeneration`] for a stale
    /// generation.
    pub fn set_demand<S, A>(
        &self,
        server_connection_id: &S,
        attachment_id: &A,
        attached: bool,
    ) -> Result<(), WorkerLifecycleError>
    where
        S: WorkerIdentifier + ?Sized,
        A: WorkerIdentifier + ?Sized,
    {
        let server_connection_id = server_connection_id.to_js_string();
        let attachment_id = attachment_id.to_js_string();
        expire_due(&self.inner, None);
        let should_retire = {
            let mut state = lock(&self.inner.state);
            if state.closed {
                return Err(WorkerLifecycleError::Closed);
            }
            if state.retiring {
                return Err(WorkerLifecycleError::Retiring);
            }
            if state.current_server_connection_id.as_ref() != Some(&server_connection_id) {
                return Err(WorkerLifecycleError::StaleDemandServerGeneration);
            }
            state.demand_initialized = true;
            state.initial_deadline = None;
            let key = DemandKey {
                server_connection_id,
                attachment_id,
            };
            state.demands.remove(&key);
            if attached {
                state.demands.insert(
                    key,
                    Demand {
                        orphan_deadline: None,
                    },
                );
            }
            reconcile_locked(&mut state)
        };
        self.inner.wake.notify_one();
        if should_retire {
            (self.inner.on_retire)();
        }
        Ok(())
    }

    /// Protects the worker from retirement for one Harness operation.
    pub fn operation_started<S, I>(&self, kind: OperationKind, lane: &S, operation_id: &I)
    where
        S: WorkerIdentifier + ?Sized,
        I: WorkerIdentifier + ?Sized,
    {
        let lane = lane.to_js_string();
        let operation_id = operation_id.to_js_string();
        expire_due(&self.inner, None);
        {
            let mut state = lock(&self.inner.state);
            if state.closed || state.retiring {
                return;
            }
            state.active_operations.insert(OperationKey {
                kind: operation_tag(kind),
                lane,
                operation_id,
            });
        }
        self.inner.wake.notify_one();
    }

    /// Ends one Harness operation and re-runs retirement reconciliation.
    pub fn operation_stopped<S, I>(&self, kind: OperationKind, lane: &S, operation_id: &I)
    where
        S: WorkerIdentifier + ?Sized,
        I: WorkerIdentifier + ?Sized,
    {
        let lane = lane.to_js_string();
        let operation_id = operation_id.to_js_string();
        expire_due(&self.inner, None);
        let should_retire = {
            let mut state = lock(&self.inner.state);
            if state.closed {
                return;
            }
            state.active_operations.remove(&OperationKey {
                kind: operation_tag(kind),
                lane,
                operation_id,
            });
            reconcile_locked(&mut state)
        };
        self.inner.wake.notify_one();
        if should_retire {
            (self.inner.on_retire)();
        }
    }

    /// Requests terminal lifecycle shutdown and cancels its outstanding
    /// deadlines. Closing never invokes the retirement callback.
    pub fn close(&self) {
        {
            let mut state = lock(&self.inner.state);
            if state.closed {
                return;
            }
            state.closed = true;
            state.initial_deadline = None;
            state.demands.clear();
            state.active_operations.clear();
            state.retirement_holds = 0;
        }
        self.inner.wake.notify_one();
    }

    /// Closes the lifecycle and waits for its owned timer task to finish.
    pub async fn close_and_wait(&self) {
        self.close();
        self.wait_for_timer().await;
    }

    /// Closes the lifecycle and waits for its owned timer task to finish.
    pub async fn closed(&self) {
        self.close_and_wait().await;
    }

    /// Returns whether retirement has won the lifecycle race.
    #[must_use]
    pub fn is_retiring(&self) -> bool {
        lock(&self.inner.state).retiring
    }

    /// Returns whether terminal close has been requested.
    #[must_use]
    pub fn is_closed(&self) -> bool {
        lock(&self.inner.state).closed
    }

    async fn wait_for_timer(&self) {
        loop {
            let notified = self.inner.closed_notify.notified();
            if lock(&self.inner.state).timer_finished {
                return;
            }
            notified.await;
        }
    }
}

impl Drop for WorkerLifecycle {
    fn drop(&mut self) {
        self.close();
        if let Some(timer) = lock(&self.inner.timer).as_ref() {
            timer.abort();
        }
    }
}

fn operation_tag(kind: OperationKind) -> u8 {
    match kind {
        OperationKind::Run => 0,
        OperationKind::Compaction => 1,
        OperationKind::Navigation => 2,
    }
}

impl State {
    fn next_deadline(&self) -> Option<Deadline> {
        let mut next = self.initial_deadline;
        for demand in self.demands.values() {
            let Some(candidate) = demand.orphan_deadline else {
                continue;
            };
            if next.is_none_or(|current| candidate.at < current.at) {
                next = Some(candidate);
            }
        }
        next
    }
}

fn reconcile_locked(state: &mut State) -> bool {
    if state.closed
        || state.retiring
        || !state.demand_initialized
        || state.retirement_holds != 0
        || !state.active_operations.is_empty()
        || !state.demands.is_empty()
    {
        return false;
    }
    state.retiring = true;
    true
}

fn release_hold(inner: &Arc<Inner>) {
    let should_retire = {
        let mut state = lock(&inner.state);
        if state.retirement_holds == 0 || state.closed {
            false
        } else {
            state.retirement_holds -= 1;
            reconcile_locked(&mut state)
        }
    };
    inner.wake.notify_one();
    if should_retire {
        (inner.on_retire)();
    }
}

async fn timer_loop(inner: Weak<Inner>, wake: Arc<Notify>) {
    loop {
        let notified = wake.notified();
        let Some(strong) = inner.upgrade() else {
            return;
        };
        let (closed, deadline) = {
            let state = lock(&strong.state);
            (state.closed || state.retiring, state.next_deadline())
        };
        if closed {
            finish_timer(&strong);
            return;
        }
        match deadline {
            Some(deadline) => {
                tokio::select! {
                    () = tokio::time::sleep_until(deadline.at) => {
                        expire_due(&strong, Some(deadline.token));
                    }
                    () = notified => {}
                }
            }
            None => {
                notified.await;
            }
        }
    }
}

fn expire_due(inner: &Arc<Inner>, token: Option<u64>) {
    let now = Instant::now();
    let should_retire = {
        let mut state = lock(&inner.state);
        if state.closed || state.retiring {
            false
        } else {
            if state.initial_deadline.is_some_and(|deadline| {
                deadline.at <= now && token.is_none_or(|candidate| candidate == deadline.token)
            }) {
                state.initial_deadline = None;
                state.demand_initialized = true;
            }
            state.demands.retain(|_, demand| {
                demand.orphan_deadline.is_none_or(|deadline| {
                    deadline.at > now || token.is_some_and(|candidate| candidate != deadline.token)
                })
            });
            reconcile_locked(&mut state)
        }
    };
    if should_retire {
        (inner.on_retire)();
    }
}

fn finish_timer(inner: &Arc<Inner>) {
    {
        let mut state = lock(&inner.state);
        state.timer_finished = true;
    }
    inner.closed_notify.notify_one();
}

fn next_timer_token(token: u64) -> u64 {
    token.checked_add(1).unwrap_or(1)
}

/// Resolves one lifecycle delay using JavaScript `Number`-like conversion.
///
/// `None` selects `fallback_ms`; an empty supplied string is JavaScript's
/// numeric zero. Supplied values must be finite, integral, non-negative, and
/// no larger than `Number.MAX_SAFE_INTEGER`.
///
/// # Errors
/// Returns [`WorkerLifecycleError::InvalidDelay`] when the value is not a
/// non-negative safe integer or when the fallback is outside that domain.
pub fn lifecycle_delay(
    name: &str,
    value: Option<&str>,
    fallback_ms: u64,
) -> Result<Duration, WorkerLifecycleError> {
    let Some(value) = value else {
        return delay_from_u64(name, fallback_ms);
    };
    let value = value.trim();
    if value.is_empty() {
        return Ok(Duration::ZERO);
    }
    if let Some(integer) = prefixed_integer(value) {
        return delay_from_u64(name, integer);
    }
    let parsed = value.parse::<f64>().map_err(|_| invalid_delay(name))?;
    if !parsed.is_finite()
        || parsed < 0.0
        || parsed.fract() != 0.0
        || parsed > MAX_SAFE_INTEGER_MILLIS_F64
    {
        return Err(invalid_delay(name));
    }
    #[expect(
        clippy::cast_possible_truncation,
        reason = "finite integral value is bounded by Number.MAX_SAFE_INTEGER"
    )]
    #[expect(
        clippy::cast_sign_loss,
        reason = "negative values are rejected before conversion"
    )]
    let millis = parsed as u64;
    Ok(Duration::from_millis(millis))
}

/// Resolves one lifecycle delay from an environment variable.
///
/// A missing variable selects `fallback_ms`. Present values use
/// [`lifecycle_delay`], including its empty-string and safe-integer rules.
///
/// # Errors
///
/// Returns [`WorkerLifecycleError`] when the variable holds a non-unicode,
/// empty, or out-of-range delay.
pub fn lifecycle_delay_from_env(
    name: &str,
    fallback_ms: u64,
) -> Result<Duration, WorkerLifecycleError> {
    match std::env::var(name) {
        Ok(value) => lifecycle_delay(name, Some(&value), fallback_ms),
        Err(std::env::VarError::NotPresent) => lifecycle_delay(name, None, fallback_ms),
        Err(std::env::VarError::NotUnicode(_)) => Err(invalid_delay(name)),
    }
}

/// Returns a duration when `millis` is within the safe-integer bound.
///
/// # Errors
/// Returns [`WorkerLifecycleError::InvalidDelay`] when `millis` exceeds
/// [`MAX_SAFE_INTEGER_MILLIS`].
fn delay_from_u64(name: &str, millis: u64) -> Result<Duration, WorkerLifecycleError> {
    if millis > MAX_SAFE_INTEGER_MILLIS {
        return Err(invalid_delay(name));
    }
    Ok(Duration::from_millis(millis))
}

fn invalid_delay(name: &str) -> WorkerLifecycleError {
    WorkerLifecycleError::InvalidDelay {
        name: name.to_owned(),
    }
}

fn prefixed_integer(value: &str) -> Option<u64> {
    let (radix, digits) = if let Some(digits) = value.strip_prefix("0x") {
        (16, digits)
    } else if let Some(digits) = value.strip_prefix("0X") {
        (16, digits)
    } else if let Some(digits) = value.strip_prefix("0b") {
        (2, digits)
    } else if let Some(digits) = value.strip_prefix("0B") {
        (2, digits)
    } else if let Some(digits) = value.strip_prefix("0o") {
        (8, digits)
    } else {
        let digits = value.strip_prefix("0O")?;
        (8, digits)
    };
    (!digits.is_empty())
        .then(|| u64::from_str_radix(digits, radix).ok())
        .flatten()
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    match mutex.lock() {
        Ok(guard) => guard,
        Err(poisoned) => poisoned.into_inner(),
    }
}
