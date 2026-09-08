//! Durable lane implementation and public `AgentLane` delegation.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use crate::context::Context;
use crate::message::AgentMessage;
use crate::session::address::{pending_assistant_frames, pending_tool_output};
use crate::session::traits::{Branch, SessionReaderExt};
use crate::session::{
    BranchScan, Entry, EntryId, LaneConfiguration, LaneName, ModelIdentity, OperationId,
    OperationResultRecord, SessionError, ToolCallStatus, Write,
};
use futures::future::BoxFuture;
use tokio::sync::{Mutex as AsyncMutex, Notify};
use tokio_util::sync::CancellationToken;

use super::harness::HarnessRuntime;
use super::support::{
    LaneData, assistant_message, custom_entry_write, entry_write, map_session_error, new_entry_id,
    operation_kind, pending_entry_write, pending_write, read_pending, sealed_rejection, set_json,
};
use crate::harness::api::{
    AgentLane, DriveOptions, IdleJob, NavigateOptions, OperationRequest, PromptInput, QueueInput,
    RecordUsageOptions,
};
use crate::harness::bus::WatchHandle;
use crate::harness::event::{HarnessEvent, HarnessEventPayload};
use crate::harness::gate::{Gate, GateControl, create_gate};
use crate::harness::result::{
    AbortOutcome, AbortRequestResult, AbortResult, CancelQueuedResult, CompactionOutcome,
    CompactionResult, CurrentOperationInfo, DriveOutcome, DriveResult, HarnessError, HarnessFault,
    LaneExecutionInfo, LaneInfo, NavigationOutcome, NavigationResult, OperationAdmissionResult,
    OperationStatus, QueueResult, RecordUsageResult, ResumeResult, RunOutcome, RunResult,
    SuspendedRun,
};
use crate::harness::snapshot::{
    LaneQueuedItem, LaneSnapshot, LaneSnapshotDeferred, LaneSnapshotOperation, LaneSnapshotRetry,
    LaneSnapshotTool,
};

/// Controls and gate for one synchronously-admitted drive.
pub(crate) struct DriveController {
    pub(crate) operation_id: OperationId,
    pub(crate) gate: Gate,
    pub(crate) control: GateControl,
    pub(crate) close_signal: CancellationToken,
    pub(crate) done: Arc<Notify>,
}

impl DriveController {
    pub(crate) fn new(operation_id: OperationId) -> Arc<Self> {
        let (gate, control) = create_gate();
        Arc::new(Self {
            operation_id,
            gate,
            control,
            close_signal: CancellationToken::new(),
            done: Arc::new(Notify::new()),
        })
    }
}

/// Concrete durable lane owned by a harness.
pub(crate) struct LaneRuntime {
    pub(crate) owner: Arc<HarnessRuntime>,
    pub(crate) name: LaneName,
    pub(crate) data: AsyncMutex<LaneData>,
    pub(crate) active_drive: AsyncMutex<Option<Arc<DriveController>>>,
    pub(crate) idle: Notify,
    pub(crate) state_changed: Notify,
    pub(crate) sealed: AtomicBool,
    /// First sealing fault recorded for this lane. Kept behind a synchronous
    /// mutex so `seal` never awaits `data` while a caller holds that guard
    /// across a durable commit.
    pub(crate) fault: Mutex<Option<Arc<HarnessFault>>>,
}

impl LaneRuntime {
    pub(crate) fn new(owner: Arc<HarnessRuntime>, name: LaneName, data: LaneData) -> Arc<Self> {
        Arc::new(Self {
            owner,
            name,
            data: AsyncMutex::new(data),
            active_drive: AsyncMutex::new(None),
            idle: Notify::new(),
            state_changed: Notify::new(),
            sealed: AtomicBool::new(false),
            fault: Mutex::new(None),
        })
    }

    pub(crate) fn is_closed(&self) -> bool {
        self.sealed.load(Ordering::Acquire) || self.owner.is_closed()
    }

    pub(crate) fn ensure_open(&self) -> Result<(), HarnessError> {
        if self.is_closed() {
            if let Some(fault) = self.owner.fault.lock().ok().and_then(|slot| slot.clone()) {
                return Err(crate::harness::runtime::support::sealed_rejection(&fault));
            }
            return Err(self.owner.closed_error());
        }
        Ok(())
    }
    pub(crate) async fn seal(&self, fault: Arc<HarnessFault>) {
        self.sealed.store(true, Ordering::Release);
        if let Ok(mut slot) = self.fault.lock()
            && slot.is_none()
        {
            *slot = Some(Arc::clone(&fault));
        }
        if let Some(drive) = self.active_drive.lock().await.as_ref() {
            drive.close_signal.cancel();
            drive.control.close(Arc::clone(&fault));
        }
        self.state_changed.notify_waiters();
        self.idle.notify_waiters();
    }

    pub(crate) async fn branch(&self, cx: &Context) -> Result<Arc<dyn Branch>, HarnessError> {
        self.owner
            .session
            .branch(&self.name, cx)
            .await
            .map_err(map_session_error)?
            .ok_or_else(|| HarnessError::InvalidLane {
                lane: self.name.clone(),
                reason: "branch_missing".to_owned(),
                message: "lane branch is missing from session".to_owned(),
            })
    }
    pub(crate) async fn commit(
        &self,
        writes: Vec<Write>,
        cx: &Context,
    ) -> Result<crate::session::CommitResult, HarnessError> {
        let mutator = self
            .owner
            .session
            .begin_mutation(cx)
            .await
            .map_err(map_session_error)?;
        match mutator.commit(writes, cx).await {
            Ok(result) => Ok(result),
            Err(error) => Err(self.commit_fault(error, cx).await),
        }
    }

    /// Returns the fault that sealed this lane, when one was recorded.
    pub(crate) fn sealed_fault(&self) -> Option<Arc<HarnessFault>> {
        self.fault.lock().ok().and_then(|slot| slot.clone())
    }

    /// Routes a durable-commit failure through the shared fault boundary.
    ///
    /// The first recorded fault wins: when one is already stored the harness
    /// is sealed and broadcasting again would only duplicate the fault event,
    /// so the stored fault is reused for the rejection. Callers may hold the
    /// lane `data` guard across the commit; the broadcast never awaits it
    /// because `seal` records the lane fault in a synchronous slot.
    pub(crate) async fn commit_fault(&self, error: SessionError, cx: &Context) -> HarnessError {
        if self
            .owner
            .fault
            .lock()
            .ok()
            .and_then(|slot| slot.clone())
            .is_none()
        {
            self.owner
                .fault(
                    HarnessFault {
                        message: format!("session operation failed: {error}"),
                        cause: Box::new(error),
                    },
                    cx,
                )
                .await;
        }
        match self.owner.fault.lock().ok().and_then(|slot| slot.clone()) {
            Some(fault) => sealed_rejection(&fault),
            None => self.owner.closed_error(),
        }
    }

    pub(crate) async fn emit(&self, payload: HarnessEventPayload, cx: &Context) {
        self.owner
            .emit(HarnessEvent::lane(self.name.clone(), payload), cx)
            .await;
    }

    pub(crate) async fn current_operation(&self) -> Option<crate::session::Operation> {
        self.data.lock().await.operation.clone()
    }

    pub(crate) async fn info(&self, _cx: &Context) -> Result<LaneInfo, HarnessError> {
        self.ensure_open()?;
        let data = self.data.lock().await;
        let operation = data.operation.as_ref().map(|operation| {
            let status = match operation.state.scope().control {
                crate::session::Control::CancelRequested { .. } => OperationStatus::Aborting,
                crate::session::Control::Running => OperationStatus::Open,
            };
            CurrentOperationInfo {
                id: operation.meta.operation_id.clone(),
                kind: operation_kind(&operation.meta.intent),
                started_at: operation.meta.started_at,
                status,
                captured_model: captured_model(&operation.state),
            }
        });
        Ok(LaneInfo {
            name: self.name.clone(),
            tip_id: data.tip.clone(),
            operation,
        })
    }

    pub(crate) async fn execution_info(
        &self,
        _cx: &Context,
    ) -> Result<LaneExecutionInfo, HarnessError> {
        self.ensure_open()?;
        let data = self.data.lock().await;
        let current = data
            .operation
            .as_ref()
            .map(|operation| CurrentOperationInfo {
                id: operation.meta.operation_id.clone(),
                kind: operation_kind(&operation.meta.intent),
                started_at: operation.meta.started_at,
                status: match &operation.state.scope().control {
                    crate::session::Control::CancelRequested { .. } => OperationStatus::Aborting,
                    crate::session::Control::Running => OperationStatus::Open,
                },
                captured_model: captured_model(&operation.state),
            });
        Ok(LaneExecutionInfo {
            lane: self.name.clone(),
            tip_id: data.tip.clone(),
            configured_model: data.config.model.clone(),
            current,
            last_operation_id: data.state.last_operation_id.clone(),
        })
    }

    pub(crate) async fn snapshot(&self, cx: &Context) -> Result<LaneSnapshot, HarnessError> {
        self.ensure_open()?;
        let data = self.data.lock().await.clone();
        let branch = self.branch(cx).await?;
        let transcript = super::support::branch_entries(branch.as_ref(), cx)
            .await
            .map_err(map_session_error)?;
        let queues = read_queue_snapshot(self, &data.state.inbox, cx).await?;
        let operation = match data.operation.as_ref() {
            Some(operation) => Some(
                snapshot_operation(self.owner.session.as_ref(), operation, &transcript, cx).await?,
            ),
            None => None,
        };
        Ok(LaneSnapshot {
            lane: self.name.clone(),
            transcript,
            tip_id: data.tip,
            last_result: data.last_result,
            configuration: data.config,
            stats: self
                .owner
                .session
                .get_stats(cx)
                .await
                .map_err(map_session_error)?,
            operation,
            queues,
            faulted: self.sealed_fault().is_some(),
        })
    }
}

impl AgentLane for LaneRuntime {
    fn name(&self) -> &LaneName {
        &self.name
    }

    fn get_tip_id<'a>(
        &'a self,
        _cx: &'a Context,
    ) -> BoxFuture<'a, Result<Option<EntryId>, HarnessError>> {
        Box::pin(async move {
            self.ensure_open()?;
            Ok(self.data.lock().await.tip.clone())
        })
    }

    fn find_entries<'a>(
        &'a self,
        query: Option<&'a BranchScan>,
        cx: &'a Context,
    ) -> BoxFuture<'a, Result<Vec<Entry>, HarnessError>> {
        Box::pin(async move {
            self.ensure_open()?;
            self.branch(cx)
                .await?
                .find_entries(query, cx)
                .await
                .map_err(map_session_error)
        })
    }

    fn find_entry<'a>(
        &'a self,
        query: Option<&'a BranchScan>,
        cx: &'a Context,
    ) -> BoxFuture<'a, Result<Option<Entry>, HarnessError>> {
        Box::pin(async move {
            self.ensure_open()?;
            self.branch(cx)
                .await?
                .find_entry(query, cx)
                .await
                .map_err(map_session_error)
        })
    }

    fn get_result<'a>(
        &'a self,
        operation_id: &'a OperationId,
        cx: &'a Context,
    ) -> BoxFuture<'a, Result<Option<OperationResultRecord>, HarnessError>> {
        Box::pin(async move {
            self.ensure_open()?;
            self.owner
                .session
                .get_value(&crate::session::address::operation_result(operation_id), cx)
                .await
                .map_err(map_session_error)
                .map(|value| value.map(|stored| stored.value))
        })
    }

    fn append_message<'a>(
        &'a self,
        message: AgentMessage,
        cx: &'a Context,
    ) -> BoxFuture<'a, Result<EntryId, HarnessError>> {
        Box::pin(async move { self.append_message_impl(message, cx).await })
    }

    fn append_custom_entry<'a>(
        &'a self,
        custom_type: &'a str,
        data: Option<serde_json::Value>,
        cx: &'a Context,
    ) -> BoxFuture<'a, Result<EntryId, HarnessError>> {
        Box::pin(async move { self.append_custom_impl(custom_type, data, cx).await })
    }

    fn accept<'a>(
        &'a self,
        request: OperationRequest,
        cx: &'a Context,
    ) -> BoxFuture<'a, OperationAdmissionResult> {
        Box::pin(async move { super::admission::accept(self, request, cx).await })
    }

    fn drive<'a>(&'a self, options: DriveOptions, cx: &'a Context) -> BoxFuture<'a, DriveResult> {
        Box::pin(async move { super::drive::drive(self, options, cx).await })
    }

    fn request_abort<'a>(
        &'a self,
        operation_id: &'a OperationId,
        cx: &'a Context,
    ) -> BoxFuture<'a, AbortRequestResult> {
        Box::pin(async move { super::admission::request_abort(self, operation_id, cx).await })
    }

    fn inspect_execution<'a>(
        &'a self,
        cx: &'a Context,
    ) -> BoxFuture<'a, Result<LaneExecutionInfo, HarnessError>> {
        Box::pin(async move { self.execution_info(cx).await })
    }

    fn prompt<'a>(&'a self, prompt: PromptInput, cx: &'a Context) -> BoxFuture<'a, RunResult> {
        Box::pin(async move {
            let admission = super::admission::accept(
                self,
                OperationRequest::Prompt {
                    operation_id: None,
                    prompt,
                },
                cx,
            )
            .await?;
            run_to_result(self, admission.operation_id, true, false, cx).await
        })
    }

    fn skill<'a>(
        &'a self,
        name: &'a str,
        additional_instructions: Option<&'a str>,
        cx: &'a Context,
    ) -> BoxFuture<'a, RunResult> {
        Box::pin(async move {
            let request = OperationRequest::Skill {
                operation_id: None,
                name: name.to_owned(),
                additional_instructions: additional_instructions.map(str::to_owned),
            };
            let admission = super::admission::accept(self, request, cx).await?;
            run_to_result(self, admission.operation_id, true, false, cx).await
        })
    }

    fn prompt_from_template<'a>(
        &'a self,
        name: &'a str,
        args: &'a [String],
        cx: &'a Context,
    ) -> BoxFuture<'a, RunResult> {
        Box::pin(async move {
            let request = OperationRequest::PromptTemplate {
                operation_id: None,
                name: name.to_owned(),
                args: args.to_vec(),
            };
            let admission = super::admission::accept(self, request, cx).await?;
            run_to_result(self, admission.operation_id, true, false, cx).await
        })
    }

    fn compact<'a>(
        &'a self,
        custom_instructions: Option<&'a str>,
        cx: &'a Context,
    ) -> BoxFuture<'a, CompactionResult> {
        Box::pin(async move {
            let request = OperationRequest::Compaction {
                operation_id: None,
                custom_instructions: custom_instructions.map(str::to_owned),
            };
            let admission = super::admission::accept(self, request, cx).await?;
            let result = super::drive::drive(
                self,
                DriveOptions {
                    operation_id: admission.operation_id.clone(),
                    wait_for_retry: true,
                    poll_deferred: false,
                },
                cx,
            )
            .await?;
            let record = match result {
                DriveOutcome::Settled(record) => record,
                DriveOutcome::WaitingRetry {
                    operation_id,
                    not_before,
                } => {
                    return Err(HarnessError::NoActiveOperation {
                        lane: self.name.clone(),
                        message: format!("compaction {operation_id} waits until {not_before}"),
                    });
                }
                DriveOutcome::WaitingDeferred { operation_id, .. } => {
                    return Err(HarnessError::NoActiveOperation {
                        lane: self.name.clone(),
                        message: format!("compaction {operation_id} unexpectedly deferred"),
                    });
                }
            };
            Ok(CompactionOutcome {
                compaction: record,
                run: None,
            })
        })
    }

    fn navigate_tree<'a>(
        &'a self,
        target: Option<&'a EntryId>,
        options: NavigateOptions,
        cx: &'a Context,
    ) -> BoxFuture<'a, NavigationResult> {
        Box::pin(async move {
            let request = OperationRequest::Navigation {
                operation_id: None,
                target_id: target.cloned(),
                options,
            };
            let admission = super::admission::accept(self, request, cx).await?;
            let result = super::drive::drive(
                self,
                DriveOptions {
                    operation_id: admission.operation_id.clone(),
                    wait_for_retry: true,
                    poll_deferred: false,
                },
                cx,
            )
            .await?;
            let record = match result {
                DriveOutcome::Settled(record) => record,
                DriveOutcome::WaitingRetry {
                    operation_id,
                    not_before,
                } => {
                    return Err(HarnessError::NoActiveOperation {
                        lane: self.name.clone(),
                        message: format!("navigation {operation_id} waits until {not_before}"),
                    });
                }
                DriveOutcome::WaitingDeferred { operation_id, .. } => {
                    return Err(HarnessError::NoActiveOperation {
                        lane: self.name.clone(),
                        message: format!("navigation {operation_id} unexpectedly deferred"),
                    });
                }
            };
            Ok(NavigationOutcome {
                navigation: record,
                run: None,
            })
        })
    }
    fn resume<'a>(&'a self, cx: &'a Context) -> BoxFuture<'a, ResumeResult> {
        Box::pin(async move {
            let operation =
                self.current_operation()
                    .await
                    .ok_or_else(|| HarnessError::NothingToResume {
                        lane: self.name.clone(),
                        message: "lane has no suspended operation".to_owned(),
                    })?;
            if !matches!(
                &operation.state,
                crate::session::OperationState::DeferredSuspended { .. }
                    | crate::session::OperationState::DeferredEffectPending { .. }
            ) {
                return Err(HarnessError::NothingToResume {
                    lane: self.name.clone(),
                    message: "lane has no deferred operation".to_owned(),
                });
            }
            run_to_result(self, operation.meta.operation_id, true, true, cx).await
        })
    }

    fn abort<'a>(&'a self, cx: &'a Context) -> BoxFuture<'a, AbortResult> {
        Box::pin(async move {
            let operation =
                self.current_operation()
                    .await
                    .ok_or_else(|| HarnessError::NoActiveOperation {
                        lane: self.name.clone(),
                        message: "lane has no active operation".to_owned(),
                    })?;
            let request =
                super::admission::request_abort(self, &operation.meta.operation_id, cx).await?;
            super::drive::drive(
                self,
                DriveOptions {
                    operation_id: operation.meta.operation_id.clone(),
                    wait_for_retry: true,
                    poll_deferred: true,
                },
                cx,
            )
            .await?;
            Ok(AbortOutcome {
                operation_id: request.operation_id,
                steer: request.steer,
                follow_up: request.follow_up,
            })
        })
    }

    fn steer<'a>(&'a self, message: QueueInput, cx: &'a Context) -> BoxFuture<'a, QueueResult> {
        Box::pin(async move {
            super::admission::enqueue(self, message, crate::session::InboxItemKind::Steer, cx).await
        })
    }

    fn follow_up<'a>(&'a self, message: QueueInput, cx: &'a Context) -> BoxFuture<'a, QueueResult> {
        Box::pin(async move {
            super::admission::enqueue(self, message, crate::session::InboxItemKind::FollowUp, cx)
                .await
        })
    }

    fn next_run<'a>(&'a self, message: QueueInput, cx: &'a Context) -> BoxFuture<'a, QueueResult> {
        Box::pin(async move {
            super::admission::enqueue(self, message, crate::session::InboxItemKind::NextRun, cx)
                .await
        })
    }

    fn cancel_queued<'a>(
        &'a self,
        entry: &'a EntryId,
        cx: &'a Context,
    ) -> BoxFuture<'a, CancelQueuedResult> {
        Box::pin(async move { super::admission::cancel_queued(self, entry, cx).await })
    }

    fn record_usage<'a>(
        &'a self,
        usage: pi_ai::Usage,
        options: RecordUsageOptions,
        cx: &'a Context,
    ) -> BoxFuture<'a, RecordUsageResult> {
        Box::pin(async move { super::admission::record_usage(self, usage, options, cx).await })
    }

    fn wait_for_idle<'a>(&'a self, cx: &'a Context) -> BoxFuture<'a, Result<(), HarnessError>> {
        Box::pin(async move {
            loop {
                let notified = self.idle.notified();
                self.ensure_open()?;
                cx.check().map_err(|_| HarnessError::Closed {
                    message: "wait for idle cancelled".to_owned(),
                })?;
                if self.current_operation().await.is_none()
                    && self.active_drive.lock().await.is_none()
                {
                    return Ok(());
                }
                notified.await;
            }
        })
    }

    fn run_when_idle<'a>(
        &'a self,
        job: IdleJob,
        cx: &'a Context,
    ) -> BoxFuture<'a, Result<(), HarnessError>> {
        Box::pin(async move {
            self.wait_for_idle(cx).await?;
            let context = cx.clone();
            job(context).await;
            Ok(())
        })
    }

    fn get_model<'a>(
        &'a self,
        _cx: &'a Context,
    ) -> BoxFuture<'a, Result<Option<pi_ai::Model>, HarnessError>> {
        Box::pin(async move {
            self.ensure_open()?;
            let config = self.data.lock().await.config.model.clone();
            Ok(self
                .owner
                .models
                .get_model(&config.provider, &config.model_id))
        })
    }

    fn set_model<'a>(
        &'a self,
        model: ModelIdentity,
        cx: &'a Context,
    ) -> BoxFuture<'a, Result<(), HarnessError>> {
        Box::pin(async move {
            self.set_lane_configuration(|config| config.model = model, cx)
                .await
        })
    }

    fn get_thinking_level<'a>(
        &'a self,
        _cx: &'a Context,
    ) -> BoxFuture<'a, Result<pi_ai::ModelThinkingLevel, HarnessError>> {
        Box::pin(async move {
            self.ensure_open()?;
            Ok(self.data.lock().await.config.thinking_level)
        })
    }

    fn set_thinking_level<'a>(
        &'a self,
        level: pi_ai::ModelThinkingLevel,
        cx: &'a Context,
    ) -> BoxFuture<'a, Result<(), HarnessError>> {
        Box::pin(async move {
            self.set_lane_configuration(|config| config.thinking_level = level, cx)
                .await
        })
    }

    fn get_active_tools<'a>(
        &'a self,
        _cx: &'a Context,
    ) -> BoxFuture<'a, Result<Vec<String>, HarnessError>> {
        Box::pin(async move {
            self.ensure_open()?;
            Ok(self.data.lock().await.config.active_tool_names.clone())
        })
    }

    fn set_active_tools<'a>(
        &'a self,
        names: Vec<String>,
        cx: &'a Context,
    ) -> BoxFuture<'a, Result<(), HarnessError>> {
        Box::pin(async move {
            let available = {
                let guard = self.owner.config.read().await;
                guard
                    .tools
                    .iter()
                    .map(|tool| tool.name().to_owned())
                    .collect::<std::collections::BTreeSet<_>>()
            };
            if let Some(name) = names.iter().find(|name| !available.contains(name.as_str())) {
                return Err(HarnessError::InvalidLane {
                    lane: self.name.clone(),
                    reason: "unknown_tool".to_owned(),
                    message: format!("active tool {name} is not registered"),
                });
            }
            self.set_lane_configuration(|config| config.active_tool_names = names, cx)
                .await
        })
    }

    fn watch<'a>(
        &'a self,
        cx: &'a Context,
    ) -> BoxFuture<'a, Result<WatchHandle<LaneSnapshot>, HarnessError>> {
        Box::pin(async move {
            let snapshot = self.snapshot(cx).await?;
            self.owner.events.watch(snapshot, Arc::new(|_| true), None)
        })
    }
}

impl LaneRuntime {
    async fn set_lane_configuration<F>(&self, change: F, cx: &Context) -> Result<(), HarnessError>
    where
        F: FnOnce(&mut LaneConfiguration),
    {
        self.ensure_open()?;
        let mut data = self.data.lock().await;
        if let Some(operation) = data.operation.as_ref() {
            return Err(HarnessError::LaneBusy {
                lane: self.name.clone(),
                operation_id: operation.meta.operation_id.clone(),
                operation_kind: operation_kind(&operation.meta.intent),
                message: "lane configuration cannot change during an operation".to_owned(),
            });
        }
        let previous = data.config.clone();
        let mut next = previous.clone();
        change(&mut next);
        let write = set_json(&super::support::lane_config_address(&self.name), &next)
            .map_err(map_session_error)?;
        self.commit(vec![write], cx).await?;
        let event_change = if previous.model != next.model {
            crate::harness::event::ConfigUpdateChange::Model {
                value: next.model.clone(),
                previous: previous.model,
            }
        } else if previous.thinking_level != next.thinking_level {
            crate::harness::event::ConfigUpdateChange::ThinkingLevel {
                value: next.thinking_level,
                previous: previous.thinking_level,
            }
        } else {
            crate::harness::event::ConfigUpdateChange::ActiveTools {
                value: next.active_tool_names.clone(),
                previous: previous.active_tool_names,
            }
        };
        data.config = next;
        drop(data);
        self.emit(
            HarnessEventPayload::ConfigUpdate {
                change: event_change,
            },
            cx,
        )
        .await;
        Ok(())
    }

    async fn append_message_impl(
        &self,
        message: AgentMessage,
        cx: &Context,
    ) -> Result<EntryId, HarnessError> {
        self.ensure_open()?;
        if message.role() == "assistant" && super::support::assistant_pending(&message) {
            return Err(HarnessError::InvalidMessage {
                lane: self.name.clone(),
                reason: "pending_assistant".to_owned(),
                message: "pending assistant messages cannot be appended".to_owned(),
            });
        }
        let mut data = self.data.lock().await;
        let id = new_entry_id(self.owner.session.as_ref()).map_err(map_session_error)?;
        if data.operation.is_some() {
            let pending = pending_entry_write(
                &id,
                &crate::session::PendingEntry::Message { payload: message },
            )
            .map_err(map_session_error)?;
            let mut next_state = data.state.clone();
            next_state.inbox.push(pending_write(
                id.clone(),
                crate::session::InboxItemKind::Write,
            ));
            let state_write =
                set_json(&super::support::lane_state_address(&self.name), &next_state)
                    .map_err(map_session_error)?;
            self.commit(vec![pending, state_write], cx).await?;
            data.state = next_state.clone();
            let inbox = next_state.inbox;
            drop(data);
            let queues = read_queue_snapshot(self, &inbox, cx).await?;
            self.emit(HarnessEventPayload::QueueUpdate { queues }, cx)
                .await;
            return Ok(id);
        }
        let parent = data.tip.clone();
        let entry = entry_write(id.clone(), parent, message, false);
        let next_tip = Some(id.clone());
        let tip_write = set_json(
            &super::support::lane_branch_tip_address(&self.name),
            &next_tip,
        )
        .map_err(map_session_error)?;
        let commit = self.commit(vec![entry, tip_write], cx).await?;
        data.tip = next_tip;
        drop(data);
        if let Some(entry) = self
            .owner
            .session
            .get_entry(&id, cx)
            .await
            .map_err(map_session_error)?
        {
            self.emit(
                HarnessEventPayload::EntryAdded {
                    entry: entry.clone(),
                },
                cx,
            )
            .await;
            self.emit(
                HarnessEventPayload::MessageEnd {
                    run_id: None,
                    message: entry
                        .message()
                        .cloned()
                        .ok_or_else(|| HarnessError::Closed {
                            message: "appended entry is not a message".to_owned(),
                        })?,
                    entry_id: Some(id.clone()),
                },
                cx,
            )
            .await;
        }
        let _ = commit;
        Ok(id)
    }

    async fn append_custom_impl(
        &self,
        custom_type: &str,
        data_value: Option<serde_json::Value>,
        cx: &Context,
    ) -> Result<EntryId, HarnessError> {
        self.ensure_open()?;
        if custom_type.is_empty() {
            return Err(HarnessError::InvalidMessage {
                lane: self.name.clone(),
                reason: "empty_custom_type".to_owned(),
                message: "custom type cannot be empty".to_owned(),
            });
        }
        let mut data = self.data.lock().await;
        let id = new_entry_id(self.owner.session.as_ref()).map_err(map_session_error)?;
        if data.operation.is_some() {
            let pending = pending_entry_write(
                &id,
                &crate::session::PendingEntry::Custom {
                    custom_type: custom_type.to_owned(),
                    payload: data_value,
                },
            )
            .map_err(map_session_error)?;
            let mut next_state = data.state.clone();
            next_state.inbox.push(pending_write(
                id.clone(),
                crate::session::InboxItemKind::Write,
            ));
            let state_write =
                set_json(&super::support::lane_state_address(&self.name), &next_state)
                    .map_err(map_session_error)?;
            self.commit(vec![pending, state_write], cx).await?;
            let inbox = next_state.inbox.clone();
            data.state = next_state;
            drop(data);
            let queues = read_queue_snapshot(self, &inbox, cx).await?;
            self.emit(HarnessEventPayload::QueueUpdate { queues }, cx)
                .await;
            return Ok(id);
        }
        let parent = data.tip.clone();
        let entry = custom_entry_write(id.clone(), parent, custom_type.to_owned(), data_value);
        let tip = Some(id.clone());
        let tip_write = set_json(&super::support::lane_branch_tip_address(&self.name), &tip)
            .map_err(map_session_error)?;
        self.commit(vec![entry, tip_write], cx).await?;
        data.tip = tip;
        drop(data);
        if let Some(entry) = self
            .owner
            .session
            .get_entry(&id, cx)
            .await
            .map_err(map_session_error)?
        {
            self.emit(HarnessEventPayload::EntryAdded { entry }, cx)
                .await;
        }
        Ok(id)
    }
}

async fn run_to_result(
    lane: &LaneRuntime,
    operation_id: OperationId,
    wait_for_retry: bool,
    poll_deferred: bool,
    cx: &Context,
) -> RunResult {
    match super::drive::drive(
        lane,
        DriveOptions {
            operation_id: operation_id.clone(),
            wait_for_retry,
            poll_deferred,
        },
        cx,
    )
    .await?
    {
        DriveOutcome::Settled(record) => Ok(RunOutcome::Settled(record)),
        DriveOutcome::WaitingRetry {
            operation_id,
            not_before,
        } => Err(HarnessError::NoActiveOperation {
            lane: lane.name.clone(),
            message: format!("operation {operation_id} is waiting for retry until {not_before}"),
        }),
        DriveOutcome::WaitingDeferred {
            operation_id,
            deferred,
        } => Ok(RunOutcome::Suspended(SuspendedRun {
            operation_id,
            deferred,
        })),
    }
}

fn captured_model(state: &crate::session::OperationState) -> Option<ModelIdentity> {
    match state {
        crate::session::OperationState::AssistantReady {
            generation_context, ..
        }
        | crate::session::OperationState::AssistantEffectPending {
            generation_context, ..
        }
        | crate::session::OperationState::AssistantRetryWait {
            generation_context, ..
        } => Some(generation_context.configuration.model.clone()),
        crate::session::OperationState::Tools { batch, .. } => {
            Some(batch.configuration.model.clone())
        }
        crate::session::OperationState::DeferredSuspended { deferred }
        | crate::session::OperationState::DeferredEffectPending { deferred, .. } => {
            Some(deferred.configuration.model.clone())
        }
        crate::session::OperationState::SummaryReady { generation, .. }
        | crate::session::OperationState::SummaryEffectPending { generation, .. }
        | crate::session::OperationState::SummaryRetryWait { generation, .. } => {
            Some(generation.summary_context.configuration.model.clone())
        }
        _ => None,
    }
}

async fn snapshot_operation(
    session: &dyn crate::session::Session,
    operation: &crate::session::Operation,
    transcript: &[Entry],
    cx: &Context,
) -> Result<LaneSnapshotOperation, HarnessError> {
    let retry = match &operation.state {
        crate::session::OperationState::AssistantRetryWait {
            generation_context,
            retry,
            ..
        } => Some(LaneSnapshotRetry {
            attempt: retry.next_attempt.saturating_sub(1),
            max_attempts: generation_context.retry_policy.max_attempts,
            next_attempt_at: retry.not_before,
        }),
        crate::session::OperationState::SummaryRetryWait {
            generation, retry, ..
        } => Some(LaneSnapshotRetry {
            attempt: retry.next_attempt.saturating_sub(1),
            max_attempts: generation.summary_context.retry_policy.max_attempts,
            next_attempt_at: retry.not_before,
        }),
        _ => None,
    };
    let deferred = match &operation.state {
        crate::session::OperationState::DeferredSuspended { deferred }
        | crate::session::OperationState::DeferredEffectPending { deferred, .. } => transcript
            .iter()
            .find(|entry| entry.id() == &deferred.source_entry_id)
            .and_then(Entry::message)
            .and_then(assistant_message)
            .and_then(|message| message.deferred.clone())
            .map(|handle| LaneSnapshotDeferred {
                handle,
                poll: deferred.poll,
            }),
        _ => None,
    };
    // A response in flight keeps its frames staged so a watcher attaching
    // mid-stream observes the partial assistant message instead of an empty
    // baseline that later events cannot repair.
    let streaming_message = match &operation.state {
        crate::session::OperationState::AssistantEffectPending {
            response_entry_id, ..
        }
        | crate::session::OperationState::DeferredEffectPending {
            response_entry_id, ..
        } => {
            let frames = session
                .read_list(
                    &pending_assistant_frames(&operation.meta.operation_id, response_entry_id),
                    None,
                    cx,
                )
                .await
                .map_err(map_session_error)?
                .into_iter()
                .map(|element| element.value)
                .collect::<Vec<_>>();
            if frames.is_empty() {
                None
            } else {
                Some(
                    crate::harness::stream::reduce_persisted_frames(&frames)
                        .map_err(map_session_error)?,
                )
            }
        }
        _ => None,
    };
    let running_tools = match &operation.state {
        crate::session::OperationState::Tools { batch, .. } => {
            snapshot_running_tools(session, operation, batch, transcript, cx).await?
        }
        _ => Vec::new(),
    };
    Ok(LaneSnapshotOperation {
        id: operation.meta.operation_id.clone(),
        kind: operation_kind(&operation.meta.intent),
        started_at: operation.meta.started_at,
        from_tip_id: operation.meta.source_tip_id.clone(),
        status: match &operation.state.scope().control {
            crate::session::Control::CancelRequested { .. } => OperationStatus::Aborting,
            crate::session::Control::Running => OperationStatus::Open,
        },
        retry,
        deferred,
        streaming_message,
        running_tools,
    })
}

/// Rebuilds the watcher-visible tool list for an operation parked in
/// `OperationState::Tools`: calls still executing become `Running`, calls
/// whose outcome is staged but not yet committed keep that staged result, and
/// planned or already-committed calls are left to events and the transcript.
async fn snapshot_running_tools(
    session: &dyn crate::session::Session,
    operation: &crate::session::Operation,
    batch: &crate::session::ToolBatch,
    transcript: &[Entry],
    cx: &Context,
) -> Result<Vec<LaneSnapshotTool>, HarnessError> {
    let Some(message) = transcript
        .iter()
        .find(|entry| entry.id() == &batch.assistant_entry_id)
        .and_then(Entry::message)
        .and_then(assistant_message)
    else {
        return Ok(Vec::new());
    };
    let mut tools = Vec::new();
    for record in &batch.calls {
        let result = match &record.status {
            ToolCallStatus::EffectPending { .. } => Some(None),
            // The staged result is not yet committed, so the call stays
            // `Running` with its produced result rather than reporting a
            // settled state the commit may still abort.
            ToolCallStatus::OutcomeReady { .. } => Some(
                session
                    .get_value(
                        &pending_tool_output(&operation.meta.operation_id, &record.result_entry_id),
                        cx,
                    )
                    .await
                    .map_err(map_session_error)?
                    .map(|stored| stored.value.result),
            ),
            ToolCallStatus::Planned | ToolCallStatus::Completed { .. } => None,
        };
        let Some(result) = result else { continue };
        let Some(pi_ai::AssistantContent::ToolCall(call)) = usize::try_from(record.source_index)
            .ok()
            .and_then(|index| message.content.get(index))
        else {
            continue;
        };
        tools.push(LaneSnapshotTool::Running {
            tool_call_id: call.id.clone(),
            tool_name: call.name.clone(),
            args: call.arguments.clone(),
            result,
        });
    }
    Ok(tools)
}

pub(crate) async fn read_queue_snapshot(
    lane: &LaneRuntime,
    inbox: &[crate::session::InboxItem],
    cx: &Context,
) -> Result<Vec<LaneQueuedItem>, HarnessError> {
    let values = read_pending(lane.owner.session.as_ref(), inbox, cx)
        .await
        .map_err(map_session_error)?;
    let mut result = Vec::with_capacity(values.len());
    for (item, pending) in values {
        match pending {
            crate::session::PendingEntry::Message { payload } => {
                result.push(LaneQueuedItem::Message {
                    entry_id: item.entry_id,
                    kind: item.kind,
                    message: payload,
                });
            }
            crate::session::PendingEntry::Custom {
                custom_type,
                payload,
            } => result.push(LaneQueuedItem::Custom {
                entry_id: item.entry_id,
                kind: item.kind,
                custom_type,
                data: payload,
            }),
        }
    }
    Ok(result)
}
