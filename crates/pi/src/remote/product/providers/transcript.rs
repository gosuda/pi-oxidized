//! Native provider for the experimental transcript service.
//!
//! The provider keeps the native lane snapshot as its reducer working copy and
//! publishes the source-shaped [`TranscriptState`] through the canonical
//! mutable replicated-state primitive.  Event conversion deliberately happens
//! only at the publication boundary: reducers consume typed native events,
//! while remote consumers receive the filtered strict-JSON wire event.

use std::future::Future;
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};

use tokio::sync::{Mutex as AsyncMutex, Notify, oneshot};
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

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum ActivationPhase {
    Inactive,
    Activating,
    Active,
}

struct ActivationState {
    generation: u64,
    phase: ActivationPhase,
    watch: Option<Arc<WatchHandle<LaneSnapshot>>>,
}

struct RebaseCompletion {
    generation: u64,
    result: Mutex<Option<Result<(), Arc<ServiceError>>>>,
    release: Mutex<Option<oneshot::Sender<()>>>,
    done: Notify,
}

impl RebaseCompletion {
    fn new(generation: u64) -> Self {
        Self {
            generation,
            result: Mutex::new(None),
            release: Mutex::new(None),
            done: Notify::new(),
        }
    }

    fn release(&self) {
        if let Some(sender) = lock(&self.release).take() {
            let _ = sender.send(());
        }
    }
}

/// Native transcript service implementation used by session workers.
///
/// The lane is owned by the service for the complete lifetime of the provider.
/// `snapshot` is the typed reducer copy; `state` is the only published copy and
/// remains the canonical [`MutableReplicatedState`] consumed by the provider.
pub struct TranscriptService {
    state: Arc<MutableReplicatedState>,
    lane: Arc<dyn AgentLane>,
    snapshot: Mutex<Option<LaneSnapshot>>,
    lifecycle: Mutex<ActivationState>,
    rebase: Mutex<Option<RebaseHandle>>,
    rebase_completion: Mutex<Option<Arc<RebaseCompletion>>>,
    rebase_error: Mutex<Option<(u64, Arc<ServiceError>)>>,
    event_gate: AsyncMutex<()>,
}

impl std::fmt::Debug for TranscriptService {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("TranscriptService")
            .field(
                "active",
                &matches!(lock(&self.lifecycle).phase, ActivationPhase::Active),
            )
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
            lifecycle: Mutex::new(ActivationState {
                generation: 0,
                phase: ActivationPhase::Inactive,
                watch: None,
            }),
            rebase: Mutex::new(None),
            rebase_completion: Mutex::new(None),
            rebase_error: Mutex::new(None),
            event_gate: AsyncMutex::new(()),
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
        let generation = {
            let mut lifecycle = lock(&self.lifecycle);
            if !matches!(lifecycle.phase, ActivationPhase::Inactive) {
                return Err(ServiceError::local("Transcript service is already active"));
            }
            lifecycle.generation = lifecycle.generation.wrapping_add(1);
            lifecycle.phase = ActivationPhase::Activating;
            lifecycle.watch = None;
            lifecycle.generation
        };
        lock(&self.snapshot).take();
        lock(&self.rebase_error).take();

        let watched = self.lane.watch(&Context::background()).await;
        let opened = match watched {
            Ok(handle) => Arc::new(handle),
            Err(error) => {
                self.reset_activation(generation);
                return Err(map_harness_exception(error));
            }
        };
        if !self.activation_is_current(generation) {
            opened.unsubscribe();
            return Err(ServiceError::disposed(
                "Transcript service activation was disposed",
            ));
        }

        let initial = opened.snapshot();
        if let Err(error) = self.publish_snapshot((*initial).clone(), None, Context::background()) {
            opened.unsubscribe();
            self.reset_activation(generation);
            return Err(error);
        }

        let service = Arc::clone(self);
        let listener: EventListener = Arc::new(move |event, context| {
            let service = Arc::clone(&service);
            Box::pin(async move {
                if let Err(error) = service.on_event(generation, event, context).await {
                    service.latch_rebase_error(generation, error);
                }
            })
        });

        let installed = {
            let mut lifecycle = lock(&self.lifecycle);
            if lifecycle.generation == generation
                && matches!(lifecycle.phase, ActivationPhase::Activating)
            {
                lifecycle.watch = Some(Arc::clone(&opened));
                lifecycle.phase = ActivationPhase::Active;
                true
            } else {
                false
            }
        };
        if !installed {
            opened.unsubscribe();
            return Err(ServiceError::disposed(
                "Transcript service activation was disposed",
            ));
        }
        if let Err(error) = opened.start(listener) {
            self.clear_watch(generation, &opened);
            opened.unsubscribe();
            self.reset_activation(generation);
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
        let (generation, watch) = {
            let mut lifecycle = lock(&self.lifecycle);
            let generation = lifecycle.generation;
            lifecycle.generation = generation.wrapping_add(1);
            lifecycle.phase = ActivationPhase::Inactive;
            (generation, lifecycle.watch.take())
        };
        if let Some(watch) = watch {
            watch.unsubscribe();
        }

        let latched_before = self.take_rebase_error(generation);
        let completion = lock(&self.rebase_completion)
            .as_ref()
            .filter(|completion| completion.generation == generation)
            .cloned();
        if let Some(completion) = &completion {
            completion.release();
        }
        let mut rebase_result = if let Some(completion) = completion {
            self.wait_rebase(completion).await
        } else {
            Ok(())
        };
        let task = lock(&self.rebase).take();
        if let Some(task) = task {
            let joined = match task.await {
                Ok(result) => result.map_err(|error| map_shared_service_error(&error)),
                Err(error) => Err(ServiceError::handler(error)),
            };
            if rebase_result.is_ok() {
                rebase_result = joined;
            }
        }

        let latched_after = self.take_rebase_error(generation);
        let latched_error = latched_before.or(latched_after);
        match (rebase_result, latched_error) {
            (Err(error), _) => Err(error),
            (Ok(()), Some(error)) => Err(map_shared_service_error(&error)),
            (Ok(()), None) => Ok(()),
        }
    }

    async fn on_event(
        self: &Arc<Self>,
        generation: u64,
        event: HarnessEvent,
        context: Context,
    ) -> Result<(), ServiceError> {
        let _event_gate = self.event_gate.lock().await;
        if !self.activation_is_active(generation) {
            return Ok(());
        }
        let completion = lock(&self.rebase_completion)
            .as_ref()
            .filter(|completion| completion.generation == generation)
            .cloned();
        let rebasing = lock(&self.rebase).is_some();
        if let Some(completion) = completion
            && rebasing
        {
            self.wait_rebase(completion).await?;
            if !self.activation_is_active(generation) {
                return Ok(());
            }
            if let Some(error) = self.rebase_error_for(generation) {
                return Err(map_shared_service_error(&error));
            }
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

        let release_rebase = if needs_rebase {
            match self.schedule_rebase(generation, context.clone()).await {
                Ok(release) => Some(release),
                Err(error) => {
                    self.latch_rebase_error(generation, error);
                    return Err(ServiceError::local("Transcript rebase could not start"));
                }
            }
        } else {
            None
        };

        let _release_rebase = release_rebase.map(|release| RebaseRelease(Some(release)));
        if !self.activation_is_active(generation) {
            return Ok(());
        }
        self.publish_snapshot(snapshot, Some(forwarded), context)
    }

    async fn schedule_rebase(
        self: &Arc<Self>,
        generation: u64,
        context: Context,
    ) -> Result<Arc<RebaseCompletion>, ServiceError> {
        if lock(&self.rebase).is_some() {
            return Err(ServiceError::local("Transcript rebase is already active"));
        }
        let watch = lock(&self.lifecycle)
            .watch
            .as_ref()
            .map(Arc::clone)
            .ok_or_else(|| ServiceError::local("Transcript service is not active"))?;
        let completion = Arc::new(RebaseCompletion::new(generation));
        let (started_sender, started_receiver) = oneshot::channel();
        let (release_sender, release_receiver) = oneshot::channel();
        let (run_sender, run_receiver) = oneshot::channel();
        *lock(&completion.release) = Some(release_sender);
        let service = Arc::clone(self);
        let task_completion = Arc::clone(&completion);
        let task = tokio::spawn(async move {
            let _ = run_receiver.await;
            let mut resnapshot = Box::pin(watch.resnapshot(&context));
            let mut started_sender = Some(started_sender);
            let captured = futures::future::poll_fn(move |poll_context| {
                let result = resnapshot.as_mut().poll(poll_context);
                if let Some(sender) = started_sender.take() {
                    let _ = sender.send(());
                }
                result
            })
            .await;

            let result = match captured {
                Ok(snapshot) => {
                    let _ = release_receiver.await;
                    if service.activation_is_current(generation) {
                        service
                            .publish_snapshot((*snapshot).clone(), None, context.clone())
                            .map_err(Arc::new)
                    } else {
                        Ok(())
                    }
                }
                Err(error) => {
                    let _ = release_receiver.await;
                    Err(Arc::new(map_harness_exception(error)))
                }
            };
            if let Err(error) = result.as_ref() {
                service.latch_rebase_error(generation, map_shared_service_error(error));
            }
            *lock(&task_completion.result) = Some(result.clone());
            task_completion.done.notify_waiters();
            lock(&service.rebase).take();
            result
        });
        *lock(&self.rebase) = Some(task);
        *lock(&self.rebase_completion) = Some(Arc::clone(&completion));
        let _ = run_sender.send(());

        started_receiver
            .await
            .map(|()| completion)
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

    fn activation_is_current(&self, generation: u64) -> bool {
        let lifecycle = lock(&self.lifecycle);
        lifecycle.generation == generation && !matches!(lifecycle.phase, ActivationPhase::Inactive)
    }

    fn activation_is_active(&self, generation: u64) -> bool {
        let lifecycle = lock(&self.lifecycle);
        lifecycle.generation == generation && matches!(lifecycle.phase, ActivationPhase::Active)
    }

    fn reset_activation(&self, generation: u64) {
        let mut lifecycle = lock(&self.lifecycle);
        if lifecycle.generation == generation {
            lifecycle.phase = ActivationPhase::Inactive;
            lifecycle.watch = None;
        }
    }

    fn latch_rebase_error(&self, generation: u64, error: ServiceError) {
        if !self.activation_is_current(generation) {
            return;
        }
        let mut slot = lock(&self.rebase_error);
        if slot
            .as_ref()
            .is_none_or(|(current, _)| *current != generation)
        {
            *slot = Some((generation, Arc::new(error)));
        }
    }

    fn rebase_error_for(&self, generation: u64) -> Option<Arc<ServiceError>> {
        lock(&self.rebase_error)
            .as_ref()
            .filter(|(current, _)| *current == generation)
            .map(|(_, error)| Arc::clone(error))
    }

    fn take_rebase_error(&self, generation: u64) -> Option<Arc<ServiceError>> {
        let mut slot = lock(&self.rebase_error);
        if slot
            .as_ref()
            .is_some_and(|(current, _)| *current == generation)
        {
            slot.take().map(|(_, error)| error)
        } else {
            None
        }
    }

    async fn wait_rebase(&self, completion: Arc<RebaseCompletion>) -> Result<(), ServiceError> {
        loop {
            let notified = completion.done.notified();
            if let Some(result) = lock(&completion.result).clone() {
                return result.map_err(|error| map_shared_service_error(&error));
            }
            notified.await;
        }
    }

    fn clear_watch(&self, generation: u64, expected: &Arc<WatchHandle<LaneSnapshot>>) {
        let mut lifecycle = lock(&self.lifecycle);
        if lifecycle.generation == generation
            && lifecycle
                .watch
                .as_ref()
                .is_some_and(|current| Arc::ptr_eq(current, expected))
        {
            lifecycle.watch = None;
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
struct RebaseRelease(Option<Arc<RebaseCompletion>>);

impl Drop for RebaseRelease {
    fn drop(&mut self) {
        if let Some(completion) = self.0.take() {
            completion.release();
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
