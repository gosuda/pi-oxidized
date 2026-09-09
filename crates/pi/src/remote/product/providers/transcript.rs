//! Native provider for the experimental transcript service.
//!
//! The provider keeps the native lane snapshot as its reducer working copy and
//! publishes the source-shaped [`TranscriptState`] through the canonical
//! mutable replicated-state primitive.  Event conversion deliberately happens
//! only at the publication boundary: reducers consume typed native events,
//! while remote consumers receive the filtered strict-JSON wire event.

use std::future::Future;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};

use tokio::sync::oneshot;
use tokio::task::JoinHandle;

type RebaseHandle = JoinHandle<Result<(), Arc<ServiceError>>>;

use pi_agent::context::Context;
use pi_agent::harness::api::AgentLane;
use pi_agent::harness::bus::{EventListener, WatchHandle};
use pi_agent::harness::event::{ConfigUpdateChange, HarnessEvent, HarnessEventPayload};
use pi_agent::harness::snapshot::{LaneSnapshot, ReduceOutcome, reduce_lane_snapshot};
use pi_agent::service::error::ServiceError;
use pi_agent::service::provider::{ServiceImplementation, ServiceMember};
use pi_agent::service::replicated::MutableReplicatedState;
use pi_agent::service::value::{JsString, JsonValue, from_serde_json};

use crate::remote::product::services::ProductJsonConvert;
use crate::remote::product::services::transcript::{TRANSCRIPT_STATE_MEMBER, TranscriptState};

/// Native transcript service implementation used by session workers.
///
/// The lane is owned by the service for the complete lifetime of the provider.
/// `snapshot` is the typed reducer copy; `state` is the only published copy and
/// remains the canonical [`MutableReplicatedState`] consumed by the provider.
pub struct TranscriptService {
    state: Arc<MutableReplicatedState>,
    lane: Arc<dyn AgentLane>,
    snapshot: Mutex<Option<LaneSnapshot>>,
    watch: Mutex<Option<Arc<WatchHandle<LaneSnapshot>>>>,
    rebase: Mutex<Option<RebaseHandle>>,
    rebase_error: Mutex<Option<Arc<ServiceError>>>,
    activating: AtomicBool,
}

impl std::fmt::Debug for TranscriptService {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("TranscriptService")
            .field("active", &lock(&self.watch).is_some())
            .field("sequence", &self.state.sequence())
            .finish_non_exhaustive()
    }
}

impl TranscriptService {
    /// Creates an inactive transcript service with the source's empty state.
    ///
    /// # Errors
    ///
    /// Returns [`ServiceError`] if the initial state cannot be encoded.
    pub fn new(lane: Arc<dyn AgentLane>) -> Result<Arc<Self>, ServiceError> {
        let initial = TranscriptState {
            snapshot: None,
            event: None,
        }
        .into_json()?;
        Ok(Arc::new(Self {
            state: MutableReplicatedState::new(initial),
            lane,
            snapshot: Mutex::new(None),
            watch: Mutex::new(None),
            rebase: Mutex::new(None),
            rebase_error: Mutex::new(None),
            activating: AtomicBool::new(false),
        }))
    }

    /// Returns the singleton service member implementation.
    pub fn implementation(self: &Arc<Self>) -> ServiceImplementation {
        let mut implementation = ServiceImplementation::new();
        implementation.insert(
            JsString::from(TRANSCRIPT_STATE_MEMBER),
            ServiceMember::State(Arc::clone(&self.state)),
        );
        implementation
    }

    /// Captures a lane watcher, publishes its initial snapshot, and starts
    /// ordered event delivery.
    ///
    /// Lane watcher creation reports its direct
    /// [`pi_agent::harness::result::HarnessError`] as a service handler failure.
    /// It is never converted into an expected harness `Closed` result.
    ///
    /// # Errors
    ///
    /// Returns [`ServiceError`] if the watcher cannot be started or the
    /// initial snapshot cannot be published.
    pub async fn activate(self: &Arc<Self>) -> Result<(), ServiceError> {
        if self.activating.swap(true, Ordering::AcqRel) {
            return Err(ServiceError::local("Transcript service is already active"));
        }

        let watched = self.lane.watch(&Context::background()).await;
        let opened = match watched {
            Ok(handle) => Arc::new(handle),
            Err(error) => {
                self.activating.store(false, Ordering::Release);
                return Err(map_harness_exception(error));
            }
        };
        {
            let mut watch = lock(&self.watch);
            if watch.is_some() {
                opened.unsubscribe();
                self.activating.store(false, Ordering::Release);
                return Err(ServiceError::local("Transcript service is already active"));
            }
            *watch = Some(Arc::clone(&opened));
        }

        let initial = opened.snapshot();
        if let Err(error) = self.publish_snapshot((*initial).clone(), None, Context::background()) {
            self.clear_watch(&opened);
            opened.unsubscribe();
            self.activating.store(false, Ordering::Release);
            return Err(error);
        }

        let service = Arc::clone(self);
        let listener: EventListener = Arc::new(move |event, context| {
            let service = Arc::clone(&service);
            Box::pin(async move {
                if let Err(error) = service.on_event(event, context).await {
                    service.latch_rebase_error(error);
                }
            })
        });

        if let Err(error) = opened.start(listener) {
            self.clear_watch(&opened);
            opened.unsubscribe();
            self.activating.store(false, Ordering::Release);
            return Err(map_harness_exception(error));
        }

        Ok(())
    }

    /// Waits for an active rebase, then closes the watcher exactly once.
    ///
    /// # Errors
    ///
    /// Returns [`ServiceError`] if the rebase task or state publication fails.
    pub async fn dispose(&self) -> Result<(), ServiceError> {
        let rebase = lock(&self.rebase).take();
        let latched_before = lock(&self.rebase_error).take();
        let rebase_result = if let Some(task) = rebase {
            match task.await {
                Ok(result) => result.map_err(|error| map_shared_service_error(&error)),
                Err(error) => Err(ServiceError::handler(error)),
            }
        } else {
            Ok(())
        };
        let latched_after = lock(&self.rebase_error).take();
        let latched_error = latched_before.or(latched_after);
        let rebase_result = match (rebase_result, latched_error) {
            (Err(error), _) => Err(error),
            (Ok(()), Some(error)) => Err(map_shared_service_error(&error)),
            (Ok(()), None) => Ok(()),
        };

        let watch = lock(&self.watch).take();
        if let Some(watch) = watch {
            watch.unsubscribe();
        }
        self.activating.store(false, Ordering::Release);
        rebase_result
    }

    async fn on_event(
        self: &Arc<Self>,
        event: HarnessEvent,
        context: Context,
    ) -> Result<(), ServiceError> {
        if let Some(error) = lock(&self.rebase_error).as_ref().map(Arc::clone) {
            return Err(map_shared_service_error(&error));
        }

        let Some(forwarded) = to_lane_watch_event(&event)? else {
            return Ok(());
        };

        let mut snapshot = lock(&self.snapshot)
            .as_ref()
            .cloned()
            .ok_or_else(|| ServiceError::local("Transcript service is not active"))?;
        let needs_rebase = matches!(
            reduce_lane_snapshot(&mut snapshot, &event),
            ReduceOutcome::NeedsResnapshot
        );

        // The source starts the watch-boundary capture before publishing the
        // navigation event.  `schedule_rebase` waits only until the native
        // resnapshot has entered its dropping phase; its completion remains
        // asynchronous and publishes the replacement snapshot with event null.
        let release_rebase = if needs_rebase {
            match self.schedule_rebase(context.clone()).await {
                Ok(release) => release,
                Err(error) => {
                    self.latch_rebase_error(error);
                    None
                }
            }
        } else {
            None
        };

        // The rebase task is held behind this release until the old snapshot
        // paired with the navigation event is durably published.  A drop
        // guard also releases it if state publication reports a panic.
        let _release_rebase = release_rebase.map(|sender| RebaseRelease(Some(sender)));
        self.publish_snapshot(snapshot, Some(forwarded), context)
    }

    async fn schedule_rebase(
        self: &Arc<Self>,
        context: Context,
    ) -> Result<Option<oneshot::Sender<()>>, ServiceError> {
        if lock(&self.rebase).is_some() {
            return Ok(None);
        }
        let watch = lock(&self.watch)
            .as_ref()
            .map(Arc::clone)
            .ok_or_else(|| ServiceError::local("Transcript service is not active"))?;
        let (started_sender, started_receiver) = oneshot::channel();
        let (release_sender, release_receiver) = oneshot::channel();
        let (run_sender, run_receiver) = oneshot::channel();
        let service = Arc::clone(self);
        let task = tokio::spawn(async move {
            let _ = run_receiver.await;
            // Signal only after the first poll. `WatchHandle::resnapshot`
            // enters its dropping phase before that poll returns, so the
            // navigation event cannot be overtaken by a later bus event.
            let mut resnapshot = Box::pin(watch.resnapshot(&context));
            let mut started_sender = Some(started_sender);
            let result = futures::future::poll_fn(move |poll_context| {
                let result = resnapshot.as_mut().poll(poll_context);
                if let Some(sender) = started_sender.take() {
                    let _ = sender.send(());
                }
                result
            })
            .await;

            // Source `scheduleRebase` publishes the navigation event before
            // the replacement snapshot, even when capture resolves quickly.
            let result = match result {
                Ok(snapshot) => {
                    let _ = release_receiver.await;
                    service
                        .publish_snapshot((*snapshot).clone(), None, context.clone())
                        .map_err(Arc::new)
                }
                Err(error) => {
                    let _ = release_receiver.await;
                    Err(Arc::new(map_harness_exception(error)))
                }
            };
            if let Err(error) = result.as_ref() {
                service.latch_rebase_error(map_shared_service_error(error));
            }
            if result.is_ok() {
                lock(&service.rebase).take();
            }
            result
        });
        *lock(&self.rebase) = Some(task);
        let _ = run_sender.send(());

        started_receiver
            .await
            .map(|()| Some(release_sender))
            .map_err(|error| {
                ServiceError::internal_with_source("Transcript resnapshot did not start", error)
            })
    }

    fn publish_snapshot(
        &self,
        snapshot: LaneSnapshot,
        event: Option<JsonValue>,
        context: Context,
    ) -> Result<(), ServiceError> {
        let next = TranscriptState {
            snapshot: Some(snapshot.clone()),
            event,
        }
        .into_json()?;
        self.state.with_state_mut(|state| *state = next);
        self.state.publish(context)?;
        *lock(&self.snapshot) = Some(snapshot);
        Ok(())
    }

    fn latch_rebase_error(&self, error: ServiceError) {
        let mut slot = lock(&self.rebase_error);
        if slot.is_none() {
            *slot = Some(Arc::new(error));
        }
    }

    fn clear_watch(&self, expected: &Arc<WatchHandle<LaneSnapshot>>) {
        let mut watch = lock(&self.watch);
        if watch
            .as_ref()
            .is_some_and(|current| Arc::ptr_eq(current, expected))
        {
            *watch = None;
        }
    }
}

fn to_lane_watch_event(event: &HarnessEvent) -> Result<Option<JsonValue>, ServiceError> {
    match &event.payload {
        HarnessEventPayload::HandlerError { .. }
        | HarnessEventPayload::TurnStart { .. }
        | HarnessEventPayload::TurnEnd { .. }
        | HarnessEventPayload::ValueUpdate { .. }
        | HarnessEventPayload::LaneCreated { .. } => return Ok(None),
        HarnessEventPayload::ConfigUpdate { change } => {
            if !matches!(
                change,
                ConfigUpdateChange::Model { .. }
                    | ConfigUpdateChange::ThinkingLevel { .. }
                    | ConfigUpdateChange::ActiveTools { .. }
            ) {
                return Ok(None);
            }
        }
        HarnessEventPayload::MessageUpdate { message, .. } if message.role() != "assistant" => {
            return Err(ServiceError::local(
                "Harness message_update did not carry an assistant message",
            ));
        }
        _ => {}
    }

    let mut value = from_serde_json(serde_json::to_value(event).map_err(ServiceError::handler)?);
    if matches!(&event.payload, HarnessEventPayload::MessageUpdate { .. }) {
        let JsonValue::Object(object) = &mut value else {
            return Err(ServiceError::internal(
                "serialized harness message_update was not an object",
            ));
        };
        object.remove(&JsString::from("event"));
    }
    Ok(Some(value))
}

fn map_harness_exception<E>(error: E) -> ServiceError
where
    E: std::error::Error + Send + Sync + 'static,
{
    ServiceError::handler(error)
}

fn map_shared_service_error(error: &Arc<ServiceError>) -> ServiceError {
    ServiceError::handler(SharedServiceError(Arc::clone(error)))
}
struct RebaseRelease(Option<oneshot::Sender<()>>);

impl Drop for RebaseRelease {
    fn drop(&mut self) {
        if let Some(sender) = self.0.take() {
            let _ = sender.send(());
        }
    }
}
#[derive(Debug)]
struct SharedServiceError(Arc<ServiceError>);

impl std::fmt::Display for SharedServiceError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.0.fmt(formatter)
    }
}

impl std::error::Error for SharedServiceError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        Some(self.0.as_ref())
    }
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}
