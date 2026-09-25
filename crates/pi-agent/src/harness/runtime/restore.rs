//! Durable lane and open-operation restoration.

use std::collections::BTreeSet;
use std::sync::Arc;

use crate::context::{Context, telemetry_context};
use crate::session::address::{
    branch_tip, branch_tip_inventory_prefix, lane_config, lane_state, operation_meta,
    operation_result, operation_state,
};
use crate::session::operation::{Control, Operation, OperationState};
use crate::session::traits::{Session, SessionReaderExt};
use crate::session::{EntryId, LaneName, LaneState, OperationId, SessionError};

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
            .get_value(&branch_tip(name.as_str()), cx)
            .await?
            .and_then(|value| value.value);
        let (state, tip, operation) = read_open_operation(session, cx, &name, state, tip).await?;
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

/// Resolves a lane snapshot's open operation, tolerating a settle that
/// commits between the snapshot and the operation-record reads.
///
/// A settle clears the lane pointer and deletes the operation records in one
/// commit, so a snapshot taken mid-settle can reference records that vanish
/// before they are read. The lane state is re-read to tell that race from
/// corruption: a cleared (or moved) pointer means the operation settled while
/// it was being read, and the returned state and tip are refreshed so the
/// lane restores from one post-settle view. The pointer naming records that
/// stay missing is corruption.
///
/// Returns the lane state, branch tip, and open operation to restore.
///
/// # Errors
///
/// [`SessionError::Invariant`] when the lane pointer keeps changing across
/// reads or still references records that remain missing.
async fn read_open_operation(
    session: &Arc<dyn Session>,
    cx: &Context,
    name: &LaneName,
    mut state: LaneState,
    mut tip: Option<EntryId>,
) -> Result<(LaneState, Option<EntryId>, Option<Operation>), SessionError> {
    let Some(mut operation_id) = state.current_operation_id.take() else {
        return Ok((state, tip, None));
    };
    for _ in 0..3 {
        let meta = session
            .get_value(&operation_meta(&operation_id), cx)
            .await?
            .map(|stored| stored.value);
        let operation_state = session
            .get_value(&operation_state(&operation_id), cx)
            .await?
            .map(|stored| stored.value);
        let meta_missing = meta.is_none();
        let (Some(meta), Some(operation_state)) = (meta, operation_state) else {
            match reconcile_vanished_records(
                session,
                cx,
                name,
                &mut state,
                &mut tip,
                &operation_id,
                meta_missing,
            )
            .await?
            {
                // Settled concurrently: the operation is no longer open.
                None => return Ok((state, tip, None)),
                // A newer operation owns the lane; read its records instead.
                Some(next) => operation_id = next,
            }
            continue;
        };
        if meta.operation_id != operation_id || meta.lane != *name {
            return Err(SessionError::Invariant(format!(
                "operation {operation_id} does not belong to lane {name}"
            )));
        }
        validate_state_intent(&meta.intent, &operation_state)?;
        state.current_operation_id = Some(operation_id);
        return Ok((
            state,
            tip,
            Some(Operation {
                meta,
                state: operation_state,
            }),
        ));
    }
    Err(SessionError::Invariant(format!(
        "lane {name} operation pointer kept changing during restore"
    )))
}

/// Classifies operation records that vanished between the lane snapshot and
/// their read, reconciling the lane with the settle that explains the loss.
///
/// The re-read lane pointer decides: still naming the missing records is
/// corruption. Cleared means the operation settled concurrently and `Ok(None)`
/// reports it; moved means a newer operation superseded it and `Ok(Some)`
/// carries the newer id to read. Either way the lane state and tip are
/// refreshed in place so the lane restores from one post-settle view.
///
/// # Errors
///
/// [`SessionError::Invariant`] when the lane pointer still references records
/// that remain missing.
async fn reconcile_vanished_records(
    session: &Arc<dyn Session>,
    cx: &Context,
    name: &LaneName,
    state: &mut LaneState,
    tip: &mut Option<EntryId>,
    operation_id: &OperationId,
    meta_missing: bool,
) -> Result<Option<OperationId>, SessionError> {
    // A deleted lane reads as an idle one here; the settle race is the case
    // this classification exists for.
    let mut fresh = session
        .get_value(&lane_state(name), cx)
        .await?
        .map(|stored| stored.value)
        .unwrap_or_default();
    let next = fresh.current_operation_id.take();
    if next.as_ref() == Some(operation_id) {
        let missing = if meta_missing { "metadata" } else { "state" };
        return Err(SessionError::Invariant(format!(
            "lane {name} references missing operation {missing}"
        )));
    }
    // The pointer cleared or moved: a settle restore raced with deleted the
    // records and rewrote the lane, so restore from the post-settle view.
    *state = fresh;
    *tip = session
        .get_value(&branch_tip(name.as_str()), cx)
        .await?
        .and_then(|value| value.value);
    Ok(next)
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

#[expect(clippy::expect_used, clippy::panic)]
#[cfg(test)]
mod tests {
    use std::collections::HashMap;
    use std::error::Error;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, Ordering};

    use futures::future::BoxFuture;

    use super::restore_session;
    use crate::context::Context;
    use crate::queue::QueueMode;
    use crate::session::address::{
        branch_tip, lane_config, lane_state, operation_meta, operation_result, operation_state,
    };
    use crate::session::configuration::CompactionSettings;
    use crate::session::lane_state::{LaneConfiguration, ModelIdentity};
    use crate::session::operation::{
        Control, OperationIntent, OperationKind, OperationMeta, OperationResultRecord,
        OperationScope, OperationState, RunSettings, TerminalStatus,
    };
    use crate::session::traits::Session;
    use crate::session::write::{delete_value, set_value};
    use crate::session::{
        CommitResult, Entry, EntryId, EntryScan, EntryStructure, LaneName, LaneState,
        ListReadOptions, MemoryStorage, OperationId, RawAddress, RawListElement, RawStoredValue,
        SessionError, SessionMetadata, SessionStats, Storage, StorageBackedSession,
        StorageBranchScan, UsageRow, UsageScan, UuidV7Generator, Write,
    };
    use crate::tool::ToolExecutionMode;

    const LANE: &str = "main";

    /// Storage that settles the seeded operation inside restore's metadata
    /// read, reproducing the settle/restore race deterministically: the settle
    /// commit clears the lane pointer, records the terminal result, and
    /// deletes the operation records as one batch, so the racing read
    /// observes the record's absence exactly as the race would leave it.
    struct SettleDuringMetaRead {
        inner: MemoryStorage,
        operation_id: OperationId,
        settled: AtomicBool,
    }

    impl SettleDuringMetaRead {
        fn settle_writes(&self) -> Vec<Write> {
            let id = &self.operation_id;
            let state = LaneState {
                last_operation_id: Some(id.clone()),
                ..LaneState::default()
            };
            let record = OperationResultRecord {
                operation_id: id.clone(),
                kind: OperationKind::Run,
                status: TerminalStatus::Completed,
                error: None,
                from_tip_id: None,
                tip_id: None,
                started_at: 1,
                ended_at: 2,
            };
            vec![
                set_value(&lane_state(&LaneName::from(LANE)), &state)
                    .expect("lane state serializes"),
                set_value(&operation_result(id), &record).expect("result record serializes"),
                delete_value(&operation_meta(id)),
                delete_value(&operation_state(id)),
            ]
        }
    }

    impl Storage for SettleDuringMetaRead {
        fn commit<'a>(
            &'a self,
            writes: Vec<Write>,
            cx: &'a Context,
        ) -> BoxFuture<'a, Result<CommitResult, SessionError>> {
            self.inner.commit(writes, cx)
        }

        fn get_entries<'a>(
            &'a self,
            ids: &'a [EntryId],
            cx: &'a Context,
        ) -> BoxFuture<'a, Result<HashMap<EntryId, Entry>, SessionError>> {
            self.inner.get_entries(ids, cx)
        }

        fn get_value<'a>(
            &'a self,
            address: &'a RawAddress,
            cx: &'a Context,
        ) -> BoxFuture<'a, Result<Option<RawStoredValue>, SessionError>> {
            let target = operation_meta(&self.operation_id);
            if address.namespace == target.namespace()
                && address.key == target.key()
                && !self.settled.swap(true, Ordering::SeqCst)
            {
                return Box::pin(async move {
                    self.inner.commit(self.settle_writes(), cx).await?;
                    self.inner.get_value(address, cx).await
                });
            }
            Box::pin(self.inner.get_value(address, cx))
        }

        fn scan_values<'a>(
            &'a self,
            prefix: &'a RawAddress,
            cx: &'a Context,
        ) -> BoxFuture<'a, Result<Vec<RawStoredValue>, SessionError>> {
            self.inner.scan_values(prefix, cx)
        }

        fn read_list<'a>(
            &'a self,
            address: &'a RawAddress,
            options: Option<ListReadOptions>,
            cx: &'a Context,
        ) -> BoxFuture<'a, Result<Vec<RawListElement>, SessionError>> {
            self.inner.read_list(address, options, cx)
        }

        fn scan_branch<'a>(
            &'a self,
            query: &'a StorageBranchScan,
            cx: &'a Context,
        ) -> BoxFuture<'a, Result<Vec<Entry>, SessionError>> {
            self.inner.scan_branch(query, cx)
        }

        fn scan_branch_structure<'a>(
            &'a self,
            query: &'a StorageBranchScan,
            cx: &'a Context,
        ) -> BoxFuture<'a, Result<Vec<EntryStructure>, SessionError>> {
            self.inner.scan_branch_structure(query, cx)
        }

        fn scan_entries<'a>(
            &'a self,
            query: &'a EntryScan,
            cx: &'a Context,
        ) -> BoxFuture<'a, Result<Vec<Entry>, SessionError>> {
            self.inner.scan_entries(query, cx)
        }

        fn scan_usage<'a>(
            &'a self,
            query: &'a UsageScan,
            cx: &'a Context,
        ) -> BoxFuture<'a, Result<Vec<UsageRow>, SessionError>> {
            self.inner.scan_usage(query, cx)
        }

        fn get_stats<'a>(
            &'a self,
            cx: &'a Context,
        ) -> BoxFuture<'a, Result<SessionStats, SessionError>> {
            self.inner.get_stats(cx)
        }

        fn close<'a>(&'a self, cx: &'a Context) -> BoxFuture<'a, Result<(), SessionError>> {
            self.inner.close(cx)
        }
    }

    /// Seeds one lane whose state names `operation_id` as open, plus the
    /// operation records when `with_records` is set.
    async fn seeded_session(
        cx: &Context,
        storage: Arc<dyn Storage>,
        operation_id: &OperationId,
        with_records: bool,
    ) -> Result<Arc<StorageBackedSession>, Box<dyn Error>> {
        let session = StorageBackedSession::new(
            SessionMetadata {
                id: "restore-settle-race".to_owned(),
                created_at: 1,
                storage_version: MemoryStorage::STORAGE_VERSION,
                cwd: None,
                parent_session_id: None,
                legacy_parent_session_path: None,
            },
            storage,
            Arc::new(UuidV7Generator::new()),
            None,
        );
        let lane = LaneName::from(LANE);
        session.create_branch(&lane, None, cx).await?;
        let mutator = session.begin_mutation(cx).await?;
        let config = LaneConfiguration {
            model: ModelIdentity {
                provider: "fixture".to_owned(),
                model_id: "fixture-model".to_owned(),
                api: None,
            },
            thinking_level: crate::pi_ai::ModelThinkingLevel::Off,
            active_tool_names: Vec::new(),
        };
        let state = LaneState {
            current_operation_id: Some(operation_id.clone()),
            ..LaneState::default()
        };
        let tip: Option<EntryId> = None;
        let mut writes = vec![
            set_value(&lane_config(&lane), &config)?,
            set_value(&lane_state(&lane), &state)?,
            set_value(&branch_tip(lane.as_str()), &tip)?,
        ];
        if with_records {
            let meta = OperationMeta {
                operation_id: operation_id.clone(),
                lane: lane.clone(),
                source_tip_id: None,
                started_at: 1,
                intent: OperationIntent::Run {
                    prompt_entry_ids: Vec::new(),
                },
            };
            let operation = OperationState::Starting {
                scope: OperationScope {
                    control: Control::Running,
                    settings: RunSettings {
                        compaction: CompactionSettings::default(),
                        steering_mode: QueueMode::All,
                        follow_up_mode: QueueMode::All,
                        tool_execution: ToolExecutionMode::default(),
                    },
                    latest_assistant_entry_id: None,
                },
            };
            writes.push(set_value(&operation_meta(operation_id), &meta)?);
            writes.push(set_value(&operation_state(operation_id), &operation)?);
        }
        mutator.commit(writes, cx).await?;
        Ok(session)
    }

    /// A settle committing between the lane snapshot and the operation-record
    /// reads must yield a valid restore of the settled lane, not an
    /// `Invariant` failure on a valid session.
    #[tokio::test(flavor = "current_thread")]
    async fn settle_during_restore_yields_valid_lane_without_open_operation()
    -> Result<(), Box<dyn Error>> {
        let cx = Context::background();
        let operation_id = OperationId::from("settled-during-restore");
        let storage = Arc::new(SettleDuringMetaRead {
            inner: MemoryStorage::new(),
            operation_id: operation_id.clone(),
            settled: AtomicBool::new(false),
        });
        let session = seeded_session(&cx, storage.clone(), &operation_id, true).await?;
        let session: Arc<dyn Session> = session;

        let (lanes, open) = restore_session(&session, &cx).await?;

        assert!(
            storage.settled.load(Ordering::SeqCst),
            "restore must reach the racing metadata read"
        );
        assert_eq!(lanes.len(), 1, "the racing settle must not drop the lane");
        assert!(
            open.is_empty(),
            "an operation settled during restore must not restore as open"
        );
        let lane = &lanes[0];
        assert_eq!(lane.name, LaneName::from(LANE));
        assert!(lane.data.operation.is_none());
        assert!(
            lane.data.state.current_operation_id.is_none(),
            "restore must adopt the post-settle lane state"
        );
        let last = lane
            .data
            .last_result
            .as_ref()
            .expect("the settle's result record must be visible");
        assert_eq!(last.operation_id, operation_id);
        assert_eq!(last.status, TerminalStatus::Completed);
        Ok(())
    }

    /// A lane pointer that keeps naming records which stay missing is
    /// corruption and must keep failing restoration.
    #[tokio::test(flavor = "current_thread")]
    async fn stable_pointer_with_missing_records_keeps_invariant_error()
    -> Result<(), Box<dyn Error>> {
        let cx = Context::background();
        let operation_id = OperationId::from("corrupt-pointer");
        let session =
            seeded_session(&cx, Arc::new(MemoryStorage::new()), &operation_id, false).await?;
        let session: Arc<dyn Session> = session;

        let Err(error) = restore_session(&session, &cx).await else {
            panic!("a pointer naming permanently missing records must fail restoration");
        };

        let SessionError::Invariant(message) = error else {
            panic!("unexpected error variant: {error}");
        };
        assert!(
            message.contains("missing operation metadata"),
            "unexpected error: {message}"
        );
        Ok(())
    }
}
