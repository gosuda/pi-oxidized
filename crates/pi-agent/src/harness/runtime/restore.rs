//! Durable lane and open-operation restoration.

use std::collections::BTreeSet;
use std::sync::Arc;

use crate::context::{Context, telemetry_context};
use crate::session::address::{
    branch_tip_inventory_prefix, lane_config, lane_state, operation_meta, operation_result,
    operation_state,
};
use crate::session::operation::{Control, Operation, OperationState};
use crate::session::traits::{Session, SessionReaderExt};
use crate::session::{LaneName, SessionError};

use super::support::LaneData;
use crate::harness::result::OpenOperation;

/// A restored lane plus its durable state.
pub(crate) struct RestoredLane {
    pub(crate) name: LaneName,
    pub(crate) data: LaneData,
}

/// Read every lane and operation without mutating session storage.
#[allow(clippy::too_many_lines)]
pub(crate) async fn restore_session(
    session: &Arc<dyn Session>,
    cx: &Context,
) -> Result<(Vec<RestoredLane>, Vec<OpenOperation>), SessionError> {
    let tip_values = session
        .scan_values(&branch_tip_inventory_prefix(), cx)
        .await?;
    let mut names = BTreeSet::new();
    for value in tip_values {
        if !value.address.key().is_empty() {
            names.insert(LaneName::new(value.address.key().to_owned()));
        }
    }

    let config_values = session
        .scan_values(&lane_config(&LaneName::new("")), cx)
        .await?;
    for value in config_values {
        if !value.address.key().is_empty() {
            names.insert(LaneName::new(value.address.key().to_owned()));
        }
    }
    let state_values = session
        .scan_values(&lane_state(&LaneName::new("")), cx)
        .await?;
    for value in state_values {
        if !value.address.key().is_empty() {
            names.insert(LaneName::new(value.address.key().to_owned()));
        }
    }

    let mut lanes = Vec::with_capacity(names.len());
    let mut open = Vec::new();
    for name in names {
        // A lane whose metadata commit failed leaves only the durable branch
        // tip behind and there is no delete_branch to clean it up; skip the
        // orphan so the surviving lanes still restore.
        let Some(config) = session.get_value(&lane_config(&name), cx).await? else {
            report_orphaned_lane(cx, &name, "lane configuration");
            continue;
        };
        let Some(state) = session.get_value(&lane_state(&name), cx).await? else {
            report_orphaned_lane(cx, &name, "lane state");
            continue;
        };
        let config = config.value;
        let state = state.value;
        let tip = session
            .get_value(&crate::session::address::branch_tip(name.as_str()), cx)
            .await?
            .and_then(|value| value.value);

        let operation = if let Some(operation_id) = state.current_operation_id.as_ref() {
            let meta = session
                .get_value(&operation_meta(operation_id), cx)
                .await?
                .ok_or_else(|| {
                    SessionError::Invariant(format!(
                        "lane {name} references missing operation metadata"
                    ))
                })?
                .value;
            let operation_state = session
                .get_value(&operation_state(operation_id), cx)
                .await?
                .ok_or_else(|| {
                    SessionError::Invariant(format!(
                        "lane {name} references missing operation state"
                    ))
                })?
                .value;
            if meta.operation_id != *operation_id || meta.lane != name {
                return Err(SessionError::Invariant(format!(
                    "operation {operation_id} does not belong to lane {name}"
                )));
            }
            validate_state_intent(&meta.intent, &operation_state)?;
            Some(Operation {
                meta,
                state: operation_state,
            })
        } else {
            None
        };
        let last_result = if let Some(last_id) = state.last_operation_id.as_ref() {
            session
                .get_value(&operation_result(last_id), cx)
                .await?
                .map(|value| value.value)
        } else {
            None
        };
        let mut data = LaneData::new(config, tip, state);
        data.operation = operation;
        data.last_result = last_result;
        if let Some(operation) = data.operation.as_ref() {
            open.push(OpenOperation {
                lane: name.clone(),
                operation_id: operation.meta.operation_id.clone(),
                kind: operation_kind(&operation.meta.intent),
                started_at: operation.meta.started_at,
                aborting: matches!(
                    operation.state.scope().control,
                    Control::CancelRequested { .. }
                ),
            });
        }
        lanes.push(RestoredLane { name, data });
    }
    Ok((lanes, open))
}

fn operation_kind(intent: &crate::session::OperationIntent) -> crate::session::OperationKind {
    match intent {
        crate::session::OperationIntent::Run { .. } => crate::session::OperationKind::Run,
        crate::session::OperationIntent::Compaction { .. } => {
            crate::session::OperationKind::Compaction
        }
        crate::session::OperationIntent::Navigation { .. } => {
            crate::session::OperationKind::Navigation
        }
    }
}

fn validate_state_intent(
    intent: &crate::session::OperationIntent,
    state: &OperationState,
) -> Result<(), SessionError> {
    let expected = operation_kind(intent);
    let actual = match state {
        OperationState::SummaryDeciding { .. }
        | OperationState::SummaryReady { .. }
        | OperationState::SummaryEffectPending { .. }
        | OperationState::SummaryRetryWait { .. } => match intent {
            crate::session::OperationIntent::Navigation {
                summarize: true, ..
            } => crate::session::OperationKind::Navigation,
            _ => crate::session::OperationKind::Compaction,
        },
        OperationState::NavigationReadyToCommit { .. } => crate::session::OperationKind::Navigation,
        _ => crate::session::OperationKind::Run,
    };
    if expected != actual {
        return Err(SessionError::Invariant(format!(
            "operation state does not match its {expected:?} intent"
        )));
    }
    Ok(())
}

/// Records a skipped metadata-less branch on telemetry so the dropped lane
/// stays diagnosable without failing the whole restore.
fn report_orphaned_lane(cx: &Context, name: &LaneName, missing: &str) {
    let parent = telemetry_context(cx);
    let mut attributes = crate::telemetry::SpanAttributes::new();
    attributes.insert(
        "pi.lane.name".to_owned(),
        crate::telemetry::AttributeValue::Str(name.as_str().to_owned()),
    );
    attributes.insert(
        "pi.restore.missing".to_owned(),
        crate::telemetry::AttributeValue::Str(missing.to_owned()),
    );
    let span = crate::telemetry::start_span_contained(
        &*parent,
        crate::telemetry::SpanOptions {
            name: "pi.harness.restore".to_owned(),
            attributes,
        },
    );
    crate::telemetry::set_status_contained(
        &*span,
        crate::telemetry::SpanStatus::Error {
            name: Some("orphaned_lane".to_owned()),
            message: Some(format!(
                "lane {name} has a durable branch but no {missing}; skipping"
            )),
        },
    );
}
