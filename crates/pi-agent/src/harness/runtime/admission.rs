//! Admission, queue, durable cancellation, and usage operations.

use futures::future::{ready, FutureExt};

use crate::context::Context;
use crate::message::AgentMessage;
use crate::queue::QueueMode;
use crate::session::address::{
    branch_tip, lane_state, operation_meta, operation_preparation, operation_state,
    pending_entry,
};
use crate::session::operation::{
    Control, Operation, OperationIntent, OperationKind, OperationState, ResultBoundary,
    SummaryTask,
};
use crate::session::{
    BranchScan, CompactionReason, Entry, EntryId, InboxItem, InboxItemKind, LaneName, LaneState,
    NewUsageRow, OperationId, PendingEntry, ScanOrder, Write,
};

use crate::harness::api::{OperationRequest, QueueInput, RecordUsageOptions};
use crate::harness::event::HarnessEventPayload;
use super::lane::LaneRuntime;
use crate::harness::result::{
    AbortRequestOutcome, AbortRequestResult, CancelQueuedKind, CancelQueuedResult,
    HarnessError, HarnessFault, OperationAdmission, OperationAdmissionResult, RecordUsageResult,
    QueueResult,
};
use super::support::{
    custom_entry_write, entry_write, map_session_error, new_entry_id, new_operation_id,
    new_usage_id, operation_kind, operation_scope, pending_entry_write, pending_write,
    prompt_messages, queue_message, read_pending, sealed_rejection, set_json, LaneData, RuntimeConfig,
};

/// Admit one operation without invoking a provider or tool.
pub(crate) async fn accept(
    lane: &LaneRuntime,
    request: OperationRequest,
    cx: &Context,
) -> OperationAdmissionResult {
    lane.ensure_open()?;
    match request {
        OperationRequest::Prompt { operation_id, prompt } => {
            let messages = prompt_messages(prompt).map_err(|error| with_lane(error, &lane.name))?;
            accept_run(lane, operation_id, messages, cx).await
        }
        OperationRequest::Skill {
            operation_id,
            name,
            additional_instructions,
        } => {
            let config = lane.owner.config_snapshot().await;
            let skill = config
                .resources
                .skills
                .iter()
                .find(|skill| skill.name == name)
                .ok_or_else(|| HarnessError::UnknownSkill {
                    name: name.clone(),
                    message: format!("skill {name} is not configured"),
                })?;
            if skill.disable_model_invocation {
                return Err(HarnessError::InvalidMessage {
                    lane: lane.name.clone(),
                    reason: "skill_disabled".to_owned(),
                    message: format!("skill {name} does not allow model invocation"),
                });
            }
            let text = match additional_instructions {
                Some(extra) if !extra.is_empty() => format!("{}\n\n{}", skill.content, extra),
                _ => skill.content.clone(),
            };
            accept_run(
                lane,
                operation_id,
                vec![crate::message::user_text(text, Vec::new())],
                cx,
            )
            .await
        }
        OperationRequest::PromptTemplate {
            operation_id,
            name,
            args,
        } => {
            let config = lane.owner.config_snapshot().await;
            let template = config
                .resources
                .prompt_templates
                .iter()
                .find(|template| template.name == name)
                .ok_or_else(|| HarnessError::UnknownTemplate {
                    name: name.clone(),
                    message: format!("prompt template {name} is not configured"),
                })?;
            let mut text = template.content.clone();
            for (index, arg) in args.iter().enumerate() {
                let one_based = index
                    .checked_add(1)
                    .ok_or_else(|| HarnessError::InvalidMessage {
                        lane: lane.name.clone(),
                        reason: "template_arguments".to_owned(),
                        message: "template argument index overflow".to_owned(),
                    })?;
                text = text.replace(&format!("{{{index}}}"), arg);
                text = text.replace(&format!("{{{one_based}}}"), arg);
            }
            accept_run(
                lane,
                operation_id,
                vec![crate::message::user_text(text, Vec::new())],
                cx,
            )
            .await
        }
        OperationRequest::Compaction {
            operation_id,
            custom_instructions,
        } => accept_compaction(lane, operation_id, custom_instructions, cx).await,
        OperationRequest::Navigation {
            operation_id,
            target_id,
            options,
        } => accept_navigation(lane, operation_id, target_id, options, cx).await,
    }
}

async fn accept_run(
    lane: &LaneRuntime,
    requested_id: Option<OperationId>,
    messages: Vec<AgentMessage>,
    cx: &Context,
) -> OperationAdmissionResult {
    if messages.is_empty() {
        return Err(HarnessError::InvalidMessage {
            lane: lane.name.clone(),
            reason: "empty_prompt".to_owned(),
            message: "run prompt cannot be empty".to_owned(),
        });
    }
    let config = lane.owner.config_snapshot().await;
    let data = lane.data.lock().await;
    if let Some(fault) = data.fault.clone() {
        return Err(sealed_rejection(&fault));
    }
    ensure_idle(lane, &data)?;
    let operation_id = match requested_id {
        Some(id) => id,
        None => new_operation_id(lane.owner.session.as_ref()).map_err(map_session_error)?,
    };
    if operation_id.as_str().is_empty() {
        return Err(HarnessError::InvalidMessage {
            lane: lane.name.clone(),
            reason: "operation_id".to_owned(),
            message: "operation id cannot be empty".to_owned(),
        });
    }
    let started_at = crate::message::now_millis();
    let selected = select_inbox(
        &data.state.inbox,
        config.steering_mode,
        config.follow_up_mode,
    );
    commit_run(
        lane,
        &config,
        data,
        RunAdmission {
            operation_id: operation_id.clone(),
            messages,
            selected,
            started_at,
        },
        cx,
    )
    .await?;
    lane.state_changed.notify_waiters();
    lane.emit(
        HarnessEventPayload::RunStart {
            run_id: operation_id.clone(),
            started_at,
        },
        cx,
    )
    .await;
    Ok(OperationAdmission {
        operation_id,
        kind: OperationKind::Run,
        started_at,
    })
}

/// A validated run request ready to be committed to durable state.
struct RunAdmission {
    operation_id: OperationId,
    messages: Vec<AgentMessage>,
    selected: Vec<InboxItem>,
    started_at: i64,
}

/// Materialize the selected inbox entries and the new prompt messages onto the
/// branch, then commit the run's durable operation and lane state and publish
/// both to the in-memory lane data.
async fn commit_run(
    lane: &LaneRuntime,
    config: &RuntimeConfig,
    data: tokio::sync::MutexGuard<'_, LaneData>,
    admission: RunAdmission,
    cx: &Context,
) -> Result<(), HarnessError> {
    let mutator = lane
        .owner
        .session
        .begin_mutation(cx)
        .await
        .map_err(map_session_error)?;
    let pending = read_pending(&*mutator, &admission.selected, cx)
        .await
        .map_err(map_session_error)?;
    let mut parent = data.tip.clone();
    let mut entry_ids = Vec::new();
    let mut writes = Vec::new();
    for (item, pending) in pending {
        let id = item.entry_id.clone();
        match pending {
            PendingEntry::Message { payload } => {
                entry_ids.push(id.clone());
                writes.push(entry_write(id.clone(), parent.clone(), payload, false));
            }
            PendingEntry::Custom {
                custom_type,
                payload,
            } => writes.push(custom_entry_write(id.clone(), parent.clone(), custom_type, payload)),
        }
        parent = Some(id.clone());
        writes.push(crate::session::delete_value(&pending_entry(&id)));
    }
    for message in admission.messages {
        let id = new_entry_id(lane.owner.session.as_ref()).map_err(map_session_error)?;
        entry_ids.push(id.clone());
        writes.push(entry_write(id.clone(), parent.clone(), message, false));
        parent = Some(id);
    }
    let operation = Operation {
        meta: super::support::operation_meta(
            &admission.operation_id,
            lane.name.clone(),
            data.tip.clone(),
            admission.started_at,
            OperationIntent::Run {
                prompt_entry_ids: entry_ids,
            },
        ),
        state: OperationState::Starting {
            scope: operation_scope(config),
        },
    };
    let next_state = LaneState {
        current_operation_id: Some(admission.operation_id.clone()),
        last_operation_id: data.state.last_operation_id.clone(),
        inbox: data
            .state
            .inbox
            .iter()
            .filter(|item| {
                !admission
                    .selected
                    .iter()
                    .any(|chosen| chosen.entry_id == item.entry_id)
            })
            .cloned()
            .collect(),
    };
    writes.push(set_json(&operation_meta(&admission.operation_id), &operation.meta).map_err(map_session_error)?);
    writes.push(set_json(&operation_state(&admission.operation_id), &operation.state).map_err(map_session_error)?);
    writes.push(set_json(&lane_state(&lane.name), &next_state).map_err(map_session_error)?);
    writes.push(set_json(&branch_tip(lane.name.as_str()), &parent).map_err(map_session_error)?);
    // Release the lane lock across the durable commit: fault broadcast
    // re-locks lane data, so holding the guard here would deadlock.
    drop(data);
    if let Err(error) = mutator.commit(writes, cx).await {
        lane.owner
            .fault(
                HarnessFault {
                    message: format!("session operation failed: {error}"),
                    cause: Box::new(error),
                },
                cx,
            )
            .await;
        let stored = lane.owner.fault.lock().ok().and_then(|slot| slot.clone());
        return Err(match stored {
            Some(fault) => sealed_rejection(&fault),
            None => lane.owner.closed_error(),
        });
    }
    let mut data = lane.data.lock().await;
    if let Some(fault) = data.fault.clone() {
        return Err(sealed_rejection(&fault));
    }
    if data.state.current_operation_id.is_some() {
        return Err(lane.owner.closed_error());
    }
    data.tip = parent;
    data.state = next_state;
    data.operation = Some(operation);
    data.last_result = None;
    Ok(())
}

async fn accept_compaction(
    lane: &LaneRuntime,
    requested_id: Option<OperationId>,
    custom_instructions: Option<String>,
    cx: &Context,
) -> OperationAdmissionResult {
    let config = lane.owner.config_snapshot().await;
    let mut data = lane.data.lock().await;
    ensure_idle(lane, &data)?;
    let branch = lane.branch(cx).await?;
    let entries = super::support::branch_entries(branch.as_ref(), cx)
        .await
        .map_err(map_session_error)?;
    let preparation = crate::harness::compaction::prepare_compaction(&entries, &config.compaction)
        .map_err(|error| HarnessError::NothingToCompact {
            lane: lane.name.clone(),
            message: format!("compaction preparation failed: {error}"),
        })?
        .ok_or_else(|| HarnessError::NothingToCompact {
            lane: lane.name.clone(),
            message: "lane has no eligible entries to compact".to_owned(),
        })?;
    let operation_id = match requested_id {
        Some(id) => id,
        None => new_operation_id(lane.owner.session.as_ref()).map_err(map_session_error)?,
    };
    if operation_id.as_str().is_empty() {
        return Err(HarnessError::InvalidMessage {
            lane: lane.name.clone(),
            reason: "operation_id".to_owned(),
            message: "operation id cannot be empty".to_owned(),
        });
    }
    let task_id = format!("{operation_id}:summary");
    let started_at = crate::message::now_millis();
    let meta = super::support::operation_meta(
        &operation_id,
        lane.name.clone(),
        data.tip.clone(),
        started_at,
        OperationIntent::Compaction {
            custom_instructions: custom_instructions.clone(),
        },
    );
    let operation = Operation {
        meta,
        state: OperationState::SummaryDeciding {
            scope: operation_scope(&config),
            task: SummaryTask {
                task_id: task_id.clone(),
                reason: Some(CompactionReason::Manual),
                custom_instructions,
                boundary: ResultBoundary::Finish,
            },
        },
    };
    let next_state = LaneState {
        current_operation_id: Some(operation_id.clone()),
        last_operation_id: data.state.last_operation_id.clone(),
        inbox: data.state.inbox.clone(),
    };
    let durable = preparation.to_durable();
    let writes = vec![
        set_json(&operation_meta(&operation_id), &operation.meta).map_err(map_session_error)?,
        set_json(&operation_state(&operation_id), &operation.state).map_err(map_session_error)?,
        set_json(&operation_preparation(&operation_id, &task_id), &durable).map_err(map_session_error)?,
        set_json(&lane_state(&lane.name), &next_state).map_err(map_session_error)?,
    ];
    lane.commit(writes, cx).await?;
    data.state = next_state;
    data.operation = Some(operation);
    drop(data);
    lane.state_changed.notify_waiters();
    lane.emit(
        HarnessEventPayload::CompactionStart {
            run_id: operation_id.clone(),
            reason: CompactionReason::Manual,
            started_at,
        },
        cx,
    )
    .await;
    Ok(OperationAdmission {
        operation_id,
        kind: OperationKind::Compaction,
        started_at,
    })
}

async fn accept_navigation(
    lane: &LaneRuntime,
    requested_id: Option<OperationId>,
    target_id: Option<EntryId>,
    options: crate::harness::api::NavigateOptions,
    cx: &Context,
) -> OperationAdmissionResult {
    if let Some(target) = target_id.as_ref() {
        let missing = lane
            .owner
            .session
            .get_entry(target, cx)
            .await
            .map_err(map_session_error)?
            .is_none();
        if missing {
            return Err(HarnessError::UnknownTarget {
                target_id: target.clone(),
                message: "navigation target does not exist".to_owned(),
            });
        }
    }
    let config = lane.owner.config_snapshot().await;
    let mut data = lane.data.lock().await;
    ensure_idle(lane, &data)?;
    let operation_id = match requested_id {
        Some(id) => id,
        None => new_operation_id(lane.owner.session.as_ref()).map_err(map_session_error)?,
    };
    if operation_id.as_str().is_empty() {
        return Err(HarnessError::InvalidNavigation {
            lane: lane.name.clone(),
            reason: "operation_id".to_owned(),
            message: "operation id cannot be empty".to_owned(),
        });
    }
    let started_at = crate::message::now_millis();
    commit_navigation(
        lane,
        &config,
        &mut data,
        NavigationAdmission {
            operation_id: operation_id.clone(),
            target_id: target_id.clone(),
            options,
            started_at,
        },
        cx,
    )
    .await?;
    drop(data);
    lane.state_changed.notify_waiters();
    lane.emit(
        HarnessEventPayload::NavigationStart {
            run_id: operation_id.clone(),
            target_id,
            started_at,
        },
        cx,
    )
    .await;
    Ok(OperationAdmission {
        operation_id,
        kind: OperationKind::Navigation,
        started_at,
    })
}

/// A validated navigation request ready to be committed to durable state.
struct NavigationAdmission {
    operation_id: OperationId,
    target_id: Option<EntryId>,
    options: crate::harness::api::NavigateOptions,
    started_at: i64,
}

/// Build the navigation operation — preparing the branch summary when the
/// navigation summarizes its detached span — then commit the durable state and
/// publish it to the in-memory lane data.
async fn commit_navigation(
    lane: &LaneRuntime,
    config: &RuntimeConfig,
    data: &mut LaneData,
    admission: NavigationAdmission,
    cx: &Context,
) -> Result<(), HarnessError> {
    let summarize = admission.options.summarize == Some(true);
    let intent = OperationIntent::Navigation {
        target_id: admission.target_id.clone(),
        summarize,
        label: admission.options.label.clone(),
        custom_instructions: admission.options.custom_instructions.clone(),
    };
    let meta = super::support::operation_meta(
        &admission.operation_id,
        lane.name.clone(),
        data.tip.clone(),
        admission.started_at,
        intent,
    );
    let preparation = if summarize {
        let entries = navigation_branch_entries(
            lane,
            data.tip.as_ref(),
            admission.target_id.as_ref(),
            cx,
        )
        .await?;
        let context_window = if config.model.context_window == 0 {
            128_000
        } else {
            config.model.context_window
        };
        let budget = context_window.saturating_sub(config.compaction.reserve_tokens);
        Some(crate::harness::compaction::prepare_branch_entries(
            &entries, budget,
        ))
    } else {
        None
    };
    let task_id = format!("{}:summary", admission.operation_id);
    let state = if summarize {
        let boundary = match admission.target_id.clone() {
            Some(target_id) => ResultBoundary::CommitNavigation {
                target_id,
                label: admission.options.label.clone(),
            },
            None => ResultBoundary::Finish,
        };
        OperationState::SummaryDeciding {
            scope: operation_scope(config),
            task: SummaryTask {
                task_id: task_id.clone(),
                reason: Some(CompactionReason::Manual),
                custom_instructions: admission.options.custom_instructions.clone(),
                boundary,
            },
        }
    } else {
        OperationState::NavigationReadyToCommit {
            scope: operation_scope(config),
            target_id: admission.target_id.clone(),
            label: admission.options.label.clone(),
        }
    };
    let operation = Operation { meta, state };
    let next_state = LaneState {
        current_operation_id: Some(admission.operation_id.clone()),
        last_operation_id: data.state.last_operation_id.clone(),
        inbox: data.state.inbox.clone(),
    };
    let mut writes = vec![
        set_json(&operation_meta(&admission.operation_id), &operation.meta).map_err(map_session_error)?,
        set_json(&operation_state(&admission.operation_id), &operation.state)
            .map_err(map_session_error)?,
        set_json(&lane_state(&lane.name), &next_state).map_err(map_session_error)?,
    ];
    if let Some(preparation) = preparation {
        writes.push(
            set_json(
                &operation_preparation(&admission.operation_id, &task_id),
                &preparation.to_durable(),
            )
            .map_err(map_session_error)?,
        );
    }
    lane.commit(writes, cx).await?;
    data.state = next_state;
    data.operation = Some(operation);
    Ok(())
}

async fn navigation_branch_entries(
    lane: &LaneRuntime,
    old_tip_id: Option<&EntryId>,
    target_id: Option<&EntryId>,
    cx: &Context,
) -> Result<Vec<Entry>, HarnessError> {
    let Some(old_tip_id) = old_tip_id else {
        return Ok(Vec::new());
    };
    let branch = lane.branch(cx).await?;
    if let Some(target_id) = target_id {
        return crate::harness::compaction::collect_entries_for_branch_summary(
            branch.as_ref(),
            lane.owner.session.as_ref(),
            Some(old_tip_id),
            target_id,
            cx,
        )
        .await
        .map(|result| result.entries)
        .map_err(map_session_error);
    }
    let mut entries = branch
        .find_entries(
            Some(&BranchScan {
                start: Some(old_tip_id.clone()),
                order: Some(ScanOrder::Desc),
                ..BranchScan::default()
            }),
            cx,
        )
        .await
        .map_err(map_session_error)?;
    entries.reverse();
    Ok(entries)
}

/// Queue one message without allowing it to bypass durable admission.
pub(crate) async fn enqueue(
    lane: &LaneRuntime,
    input: QueueInput,
    kind: InboxItemKind,
    cx: &Context,
) -> QueueResult {
    lane.ensure_open()?;
    let message = queue_message(input).map_err(|reason| HarnessError::InvalidMessage {
        lane: lane.name.clone(),
        reason: "invalid_queue_message".to_owned(),
        message: reason,
    })?;
    let id = new_entry_id(lane.owner.session.as_ref()).map_err(map_session_error)?;
    let pending = PendingEntry::Message { payload: message };
    let mut data = lane.data.lock().await;
    let pending_value_write =
        pending_entry_write(&id, &pending).map_err(map_session_error)?;
    let mut next_state = data.state.clone();
    next_state.inbox.push(pending_write(id.clone(), kind));
    let state_write =
        set_json(&lane_state(&lane.name), &next_state).map_err(map_session_error)?;
    lane.commit(vec![pending_value_write, state_write], cx).await?;
    data.state = next_state;
    let queues = data.state.inbox.clone();
    drop(data);
    lane.state_changed.notify_waiters();
    let snapshot = super::lane::read_queue_snapshot(lane, &queues, cx).await?;
    lane.emit(HarnessEventPayload::QueueUpdate { queues: snapshot }, cx)
        .await;
    Ok(id)
}

/// Cancel one queued item, distinguishing consumed and unknown ids.
pub(crate) async fn cancel_queued(
    lane: &LaneRuntime,
    entry: &EntryId,
    cx: &Context,
) -> CancelQueuedResult {
    lane.ensure_open()?;
    let mut data = lane.data.lock().await;
    let Some(index) = data
        .state
        .inbox
        .iter()
        .position(|item| &item.entry_id == entry)
    else {
        let known = lane
            .owner
            .session
            .get_entry(entry, cx)
            .await
            .map_err(map_session_error)?
            .is_some();
        return Ok(if known {
            CancelQueuedKind::AlreadyConsumed
        } else {
            CancelQueuedKind::NotFound
        });
    };
    let mut next_state = data.state.clone();
    next_state.inbox.remove(index);
    let state_write =
        set_json(&lane_state(&lane.name), &next_state).map_err(map_session_error)?;
    let delete_write = crate::session::delete_value(&pending_entry(entry));
    lane.commit(vec![delete_write, state_write], cx).await?;
    data.state = next_state;
    let queues = data.state.inbox.clone();
    drop(data);
    lane.state_changed.notify_waiters();
    lane.emit(
        HarnessEventPayload::QueueUpdate {
            queues: super::lane::read_queue_snapshot(lane, &queues, cx).await?,
        },
        cx,
    )
    .await;
    Ok(CancelQueuedKind::Cancelled)
}

/// Durably mark an operation for cancellation and drain steering/follow-up.
pub(crate) async fn request_abort(
    lane: &LaneRuntime,
    operation_id: &OperationId,
    cx: &Context,
) -> AbortRequestResult {
    lane.ensure_open()?;
    let mut data = lane.data.lock().await;
    let operation = data.operation.clone().ok_or_else(|| HarnessError::NoActiveOperation {
        lane: lane.name.clone(),
        message: "lane has no active operation".to_owned(),
    })?;
    if operation.meta.operation_id != *operation_id {
        return Err(HarnessError::OperationMismatch {
            lane: lane.name.clone(),
            expected_operation_id: operation_id.clone(),
            current_operation_id: Some(operation.meta.operation_id),
            last_operation_id: data.state.last_operation_id.clone(),
            message: "abort request does not name the current operation".to_owned(),
        });
    }
    if matches!(&operation.state.scope().control, Control::CancelRequested { .. }) {
        return Ok(AbortRequestOutcome {
            operation_id: operation_id.clone(),
            newly_requested: false,
            steer: Vec::new(),
            follow_up: Vec::new(),
        });
    }
    let drained_items: Vec<InboxItem> = data
        .state
        .inbox
        .iter()
        .filter(|item| matches!(item.kind, InboxItemKind::Steer | InboxItemKind::FollowUp))
        .cloned()
        .collect();
    let mutator = lane
        .owner
        .session
        .begin_mutation(cx)
        .await
        .map_err(map_session_error)?;
    let pending = read_pending(&*mutator, &drained_items, cx)
        .await
        .map_err(map_session_error)?;
    let mut steer = Vec::new();
    let mut follow_up = Vec::new();
    let mut writes = Vec::new();
    for (item, value) in pending {
        if let PendingEntry::Message { payload } = value {
            match item.kind {
                InboxItemKind::Steer => steer.push(payload),
                InboxItemKind::FollowUp => follow_up.push(payload),
                _ => {}
            }
        }
        writes.push(crate::session::delete_value(&pending_entry(&item.entry_id)));
    }
    let mut next_operation = operation.clone();
    next_operation.state.scope_mut().control = Control::CancelRequested {
        requested_at: crate::message::now_millis(),
    };
    let next_state = LaneState {
        current_operation_id: Some(operation_id.clone()),
        last_operation_id: data.state.last_operation_id.clone(),
        inbox: data
            .state
            .inbox
            .iter()
            .filter(|item| !drained_items.iter().any(|drained| drained.entry_id == item.entry_id))
            .cloned()
            .collect(),
    };
    writes.push(set_json(&operation_state(operation_id), &next_operation.state).map_err(map_session_error)?);
    writes.push(set_json(&lane_state(&lane.name), &next_state).map_err(map_session_error)?);
    mutator.commit(writes, cx).await.map_err(map_session_error)?;
    data.state = next_state;
    data.operation = Some(next_operation);
    drop(data);
    if let Some(drive) = lane.active_drive.lock().await.as_ref()
        && drive.operation_id == *operation_id
    {
        drive.control.begin_abort(ready(()).boxed().shared());
        drive.control.signal_abort();
    }
    lane.state_changed.notify_waiters();
    lane.emit(
        HarnessEventPayload::OperationAbort {
            operation_id: operation_id.clone(),
            steer: steer.clone(),
            follow_up: follow_up.clone(),
        },
        cx,
    )
    .await;
    Ok(AbortRequestOutcome {
        operation_id: operation_id.clone(),
        newly_requested: true,
        steer,
        follow_up,
    })
}

/// Append one usage row and report its durable id.
pub(crate) async fn record_usage(
    lane: &LaneRuntime,
    usage: pi_ai::Usage,
    options: RecordUsageOptions,
    cx: &Context,
) -> RecordUsageResult {
    lane.ensure_open()?;
    let id = new_usage_id(lane.owner.session.as_ref()).map_err(map_session_error)?;
    let entry_id = options.entry_id.clone();
    let details = options.details.clone();
    let row = NewUsageRow {
        id: id.clone(),
        usage: usage.clone(),
        entry_id: options.entry_id,
        adjustment: false,
        details: options.details,
    };
    let commit = lane.commit(vec![Write::Usage { row }], cx).await?;
    let seq = commit
        .seqs
        .first()
        .copied()
        .ok_or_else(|| HarnessError::Closed {
            message: "usage commit returned no sequence".to_owned(),
        })?;
    let row = crate::session::UsageRow {
        id: id.clone(),
        seq,
        usage: usage.clone(),
        entry_id,
        adjustment: false,
        details,
    };
    lane.emit(
        HarnessEventPayload::Usage {
            lane: lane.name.clone(),
            row,
            totals: usage,
        },
        cx,
    )
    .await;
    Ok(id)
}

fn ensure_idle(lane: &LaneRuntime, data: &LaneData) -> Result<(), HarnessError> {
    if let Some(operation) = data.operation.as_ref() {
        return Err(HarnessError::LaneBusy {
            lane: lane.name.clone(),
            operation_id: operation.meta.operation_id.clone(),
            operation_kind: operation_kind(&operation.meta.intent),
            message: "lane already owns an operation".to_owned(),
        });
    }
    Ok(())
}

fn select_inbox(
    inbox: &[InboxItem],
    steering_mode: QueueMode,
    follow_up_mode: QueueMode,
) -> Vec<InboxItem> {
    let mut selected = Vec::new();
    let mut steer_seen = false;
    let mut follow_seen = false;
    for item in inbox {
        let eligible = match item.kind {
            InboxItemKind::Write | InboxItemKind::NextRun => true,
            InboxItemKind::Steer => steering_mode == QueueMode::All || !steer_seen,
            InboxItemKind::FollowUp => follow_up_mode == QueueMode::All || !follow_seen,
        };
        if eligible {
            if item.kind == InboxItemKind::Steer {
                steer_seen = true;
            }
            if item.kind == InboxItemKind::FollowUp {
                follow_seen = true;
            }
            selected.push(item.clone());
        }
    }
    selected
}

fn with_lane(error: HarnessError, lane: &LaneName) -> HarnessError {
    match error {
        HarnessError::InvalidMessage { reason, message, .. } => HarnessError::InvalidMessage {
            lane: lane.clone(),
            reason,
            message,
        },
        other => other,
    }
}
