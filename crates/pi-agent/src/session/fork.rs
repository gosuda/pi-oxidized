use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use futures::future::BoxFuture;

use super::RawStoredValue;
use super::address::AddressKind;
use super::backed::validate_branch_name;
use super::entry::Entry;
use super::error::{SessionError, StorageErrorCode, StorageFailure};
use super::ids::EntryId;
use super::lane_state::LaneState;
use super::traits::{ForkOptions, ForkPosition, Storage};
use super::write::{CommittedValueWrite, CommittedWrite, UsageRow};
use crate::context::Context;
type BranchTips = Vec<(String, Option<EntryId>)>;
type SelectedEntries = HashSet<EntryId>;
type SelectedContents = (SelectedEntries, BranchTips);

/// State captured from one backend at a serialized commit boundary.
///
/// The snapshot contains entries, scalar values, and usage rows. Lists,
/// operation state, and pending state are transient or derived and are not
/// part of a fork's durable conversation history. Capture has no access to
/// [`ForkOptions`], so usage rows are always carried; only
/// [`ForkOptions::Tree`] forks copy them into the destination.
#[derive(Clone, Debug)]
pub struct ForkSourceSnapshot {
    /// Entries available to the fork selector.
    pub entries: Vec<Entry>,
    /// Current scalar slots in the source session.
    pub values: Vec<RawStoredValue>,
    /// Committed usage rows in source-sequence order.
    pub usage: Vec<UsageRow>,
    /// Whether `entries` is the complete source tree. A branch-only backend
    /// may set this to `false` when it supplies only the requested ancestry.
    pub entries_complete: bool,
}

/// Materialized state for a destination session.
///
/// Entry sequence numbers are retained from the source. Reconstructed scalar
/// values receive fresh sequence numbers above every source sequence, and
/// copied usage rows retain their source sequences, so a tree fork's usage
/// history stays aligned with its copied entries.
#[derive(Clone, Debug)]
pub struct ForkDestinationSnapshot {
    /// Selected entries in ascending source-sequence order.
    pub entries: Vec<Entry>,
    /// Reconstructed and copied scalar values in destination-sequence order.
    pub values: Vec<RawStoredValue>,
    /// Copied usage rows in source-sequence order, verbatim. Empty for
    /// branch forks; [`ForkOptions::Tree`] forks carry every source row.
    pub usage: Vec<UsageRow>,
    /// High-water mark above every destination sequence.
    pub next_seq: u64,
}

/// Storage that can capture a fork source without exposing backend internals.
pub trait ForkSource: Storage {
    /// Captures one coherent source boundary.
    fn capture_fork_source<'a>(
        &'a self,
        cx: &'a Context,
    ) -> BoxFuture<'a, Result<ForkSourceSnapshot, SessionError>>;
}

fn corrupt_tip(error: serde_json::Error) -> SessionError {
    SessionError::Backend(StorageFailure {
        code: StorageErrorCode::Corrupt,
        message: "stored branch tip decode failed".to_owned(),
        source: Some(Arc::new(error)),
    })
}

fn value_for<'a>(
    values: &'a [RawStoredValue],
    namespace: &str,
    key: &str,
) -> Option<&'a RawStoredValue> {
    values
        .iter()
        .find(|stored| stored.namespace == namespace && stored.key == key)
}

fn tip_values(values: &[RawStoredValue]) -> Result<BranchTips, SessionError> {
    let mut tips = Vec::new();
    let mut seen = HashSet::new();
    for stored in values
        .iter()
        .filter(|stored| stored.namespace == "pi.branch.tip")
    {
        if stored.kind != AddressKind::Value {
            return Err(SessionError::Invariant(format!(
                "branch tip {} is not a scalar value",
                stored.key
            )));
        }
        if !seen.insert(stored.key.clone()) {
            return Err(SessionError::Invariant(format!(
                "duplicate source branch tip {}",
                stored.key
            )));
        }
        let tip =
            serde_json::from_value::<Option<EntryId>>(stored.value.clone()).map_err(corrupt_tip)?;
        tips.push((stored.key.clone(), tip));
    }
    Ok(tips)
}

fn validate_source_snapshot(
    source: &ForkSourceSnapshot,
    entries: &HashMap<EntryId, Entry>,
    tips: &BranchTips,
    options: &ForkOptions,
) -> Result<(), SessionError> {
    let tip_keys: HashSet<&str> = tips.iter().map(|(key, _)| key.as_str()).collect();

    for stored in &source.values {
        if (stored.namespace == "pi.lane.config" || stored.namespace == "pi.lane.state")
            && !tip_keys.contains(stored.key.as_str())
        {
            return Err(SessionError::Invariant(format!(
                "source session branch {} is missing branch.tip",
                stored.key
            )));
        }
    }

    for (branch, tip) in tips {
        let configuration = value_for(&source.values, "pi.lane.config", branch);
        let state = value_for(&source.values, "pi.lane.state", branch);
        if configuration.is_some() != state.is_some() {
            return Err(SessionError::Invariant(format!(
                "source session branch {branch} has incomplete lane state"
            )));
        }
        if let ForkOptions::Branch {
            branch: requested, ..
        } = options
            && requested.as_str() == branch
            && configuration.is_none()
        {
            return Err(SessionError::Invariant(format!(
                "source branch {requested} is not a configured AgentLane"
            )));
        }
        if (source.entries_complete || matches!(options, ForkOptions::Tree { .. }))
            && let Some(tip) = tip
            && !entries.contains_key(tip)
        {
            return Err(SessionError::Invariant(format!(
                "source session branch {branch} has an unknown tip"
            )));
        }
    }
    Ok(())
}

fn select_contents(
    entries: &HashMap<EntryId, Entry>,
    tips: &BranchTips,
    options: &ForkOptions,
) -> Result<SelectedContents, SessionError> {
    let mut selected = HashSet::new();
    let mut destination_tips = Vec::new();
    match options {
        ForkOptions::Tree { .. } => {
            selected.extend(entries.keys().cloned());
            destination_tips.extend(tips.iter().cloned());
        }
        ForkOptions::Branch {
            branch,
            entry_id,
            position,
            ..
        } => {
            let source_tip = tips
                .iter()
                .find(|(name, _)| name.as_str() == branch.as_str())
                .map(|(_, tip)| tip.clone())
                .ok_or_else(|| {
                    SessionError::Invariant(format!("unknown source branch {branch}"))
                })?;
            let requested = entry_id.clone().or(source_tip.clone());
            let mut found = requested.is_none();
            let mut destination_tip = None;
            let mut current = source_tip;
            let mut visited = HashSet::new();
            while let Some(current_id) = current {
                if !visited.insert(current_id.clone()) {
                    return Err(SessionError::Invariant(
                        "cycle in source branch ancestry".to_owned(),
                    ));
                }
                let entry = entries.get(&current_id).ok_or_else(|| {
                    SessionError::Invariant(format!(
                        "corrupt source branch: missing parent {current_id}"
                    ))
                })?;
                if Some(entry.id().clone()) == requested {
                    found = true;
                    destination_tip = match position {
                        ForkPosition::Before => entry.parent_id().cloned(),
                        ForkPosition::At => Some(entry.id().clone()),
                    };
                    if matches!(position, ForkPosition::At) {
                        selected.insert(entry.id().clone());
                    }
                } else if found {
                    selected.insert(entry.id().clone());
                }
                current = entry.parent_id().cloned();
            }
            if !found {
                let requested = requested.map_or_else(|| "null".to_owned(), |id| id.to_string());
                return Err(SessionError::Invariant(format!(
                    "fork entry {requested} is not on source branch {branch}"
                )));
            }
            destination_tips.push((branch.to_string(), destination_tip));
        }
    }
    Ok((selected, destination_tips))
}

fn push_value(
    values: &mut Vec<RawStoredValue>,
    next_seq: &mut u64,
    namespace: &str,
    key: &str,
    value: serde_json::Value,
) -> Result<(), SessionError> {
    let seq = *next_seq;
    *next_seq = next_seq
        .checked_add(1)
        .ok_or_else(|| SessionError::Invariant("sequence overflow".to_owned()))?;
    values.push(RawStoredValue {
        namespace: namespace.to_owned(),
        key: key.to_owned(),
        kind: AddressKind::Value,
        value,
        seq,
    });
    Ok(())
}

#[derive(Clone, Copy)]
enum ForkScope {
    Branch,
    Tree,
}

fn copy_disposition(
    namespace: &str,
    key: &str,
    scope: ForkScope,
    selected: &HashSet<EntryId>,
) -> Result<bool, SessionError> {
    if namespace == "pi.session.name" {
        return Ok(true);
    }
    if namespace == "pi.entry.label" {
        return Ok(
            matches!(scope, ForkScope::Tree) || selected.contains(&EntryId::from(key.to_owned()))
        );
    }
    if namespace == "pi.branch.tip" || namespace == "pi.lane.config" || namespace == "pi.lane.state"
    {
        return Ok(false);
    }
    if namespace == "pi.result"
        || namespace.starts_with("pi.op.")
        || namespace.starts_with("pi.pending.")
    {
        return Ok(false);
    }
    if namespace == "pi" || namespace.starts_with("pi.") {
        return Err(SessionError::Invariant(format!(
            "unknown reserved fork namespace {namespace}"
        )));
    }
    Ok(matches!(scope, ForkScope::Tree))
}

/// Builds the complete logical state for a forked destination session.
///
/// # Errors
///
/// Returns [`SessionError::Invariant`] when the source snapshot is
/// inconsistent (unknown branch, missing parent, duplicate ids, or a sequence
/// overflow) or when a value cannot be serialized.
pub fn create_fork_snapshot(
    source: &ForkSourceSnapshot,
    options: &ForkOptions,
) -> Result<ForkDestinationSnapshot, SessionError> {
    if let ForkOptions::Branch { branch, .. } = options {
        validate_branch_name(branch)?;
    }
    let mut source_entries = HashMap::with_capacity(source.entries.len());
    for entry in &source.entries {
        if source_entries
            .insert(entry.id().clone(), entry.clone())
            .is_some()
        {
            return Err(SessionError::Invariant(format!(
                "duplicate source entry id {}",
                entry.id()
            )));
        }
    }
    let tips = tip_values(&source.values)?;
    validate_source_snapshot(source, &source_entries, &tips, options)?;
    let (selected, destination_tips) = select_contents(&source_entries, &tips, options)?;

    let mut entries: Vec<Entry> = selected
        .iter()
        .filter_map(|id| source_entries.get(id).cloned())
        .collect();
    entries.sort_by_key(Entry::seq);
    let mut seen_sequences = HashSet::new();
    for entry in &entries {
        if !seen_sequences.insert(entry.seq()) {
            return Err(SessionError::Invariant(format!(
                "duplicate source entry sequence {}",
                entry.seq()
            )));
        }
    }
    let scope = match options {
        ForkOptions::Branch { .. } => ForkScope::Branch,
        ForkOptions::Tree { .. } => ForkScope::Tree,
    };
    // Copied usage rows keep their source sequences, so the fresh-sequence
    // space for reconstructed values must start above them too.
    let max_usage_seq = if matches!(scope, ForkScope::Tree) {
        source.usage.iter().map(|row| row.seq).max().unwrap_or(0)
    } else {
        0
    };
    let mut next_seq = entries
        .iter()
        .map(Entry::seq)
        .max()
        .unwrap_or(0)
        .max(max_usage_seq)
        .checked_add(1)
        .ok_or_else(|| SessionError::Invariant("sequence overflow".to_owned()))?;
    let mut values = Vec::new();

    for (branch, tip) in destination_tips {
        let tip_value = serde_json::to_value(&tip).map_err(|error| {
            SessionError::Invariant(format!("branch tip serialization failed: {error}"))
        })?;
        push_value(
            &mut values,
            &mut next_seq,
            "pi.branch.tip",
            &branch,
            tip_value,
        )?;
        if let Some(configuration) = value_for(&source.values, "pi.lane.config", &branch) {
            values.push(RawStoredValue {
                namespace: "pi.lane.config".to_owned(),
                key: branch.clone(),
                kind: AddressKind::Value,
                value: configuration.value.clone(),
                seq: next_seq,
            });
            next_seq = next_seq
                .checked_add(1)
                .ok_or_else(|| SessionError::Invariant("sequence overflow".to_owned()))?;
            let state = serde_json::to_value(LaneState::default()).map_err(|error| {
                SessionError::Invariant(format!("lane state serialization failed: {error}"))
            })?;
            push_value(&mut values, &mut next_seq, "pi.lane.state", &branch, state)?;
        }
    }

    for stored in &source.values {
        if copy_disposition(&stored.namespace, &stored.key, scope, &selected)? {
            push_value(
                &mut values,
                &mut next_seq,
                &stored.namespace,
                &stored.key,
                stored.value.clone(),
            )?;
        }
    }

    // Usage rows are event history attributed to copied entries: a tree fork
    // copies every row verbatim — id, source sequence, and payload — while a
    // branch fork takes none.
    let mut usage = Vec::new();
    if matches!(scope, ForkScope::Tree) {
        usage.extend(source.usage.iter().cloned());
    }

    Ok(ForkDestinationSnapshot {
        entries,
        values,
        usage,
        next_seq,
    })
}

/// Converts a destination snapshot into replayable committed writes.
pub fn fork_snapshot_writes(snapshot: &ForkDestinationSnapshot) -> Vec<CommittedWrite> {
    let mut writes =
        Vec::with_capacity(snapshot.entries.len() + snapshot.values.len() + snapshot.usage.len());
    writes.extend(snapshot.entries.iter().cloned().map(CommittedWrite::Entry));
    writes.extend(snapshot.values.iter().cloned().map(|stored| {
        CommittedWrite::Value(CommittedValueWrite::Set {
            seq: stored.seq,
            namespace: stored.namespace,
            key: stored.key,
            value: stored.value,
        })
    }));
    writes.extend(snapshot.usage.iter().cloned().map(CommittedWrite::Usage));
    writes.sort_by_key(CommittedWrite::seq);
    writes
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::session::entry::{NewEntry, NewEntryBody};
    use crate::session::ids::{LaneName, UsageId};

    fn entry(id: &str, seq: u64) -> Entry {
        NewEntry {
            id: EntryId::from(id.to_owned()),
            parent_id: None,
            body: NewEntryBody::Custom {
                custom_type: "note".to_owned(),
                data: None,
            },
        }
        .materialize(seq, 1)
    }

    fn scalar(namespace: &str, key: &str, value: serde_json::Value, seq: u64) -> RawStoredValue {
        RawStoredValue {
            namespace: namespace.to_owned(),
            key: key.to_owned(),
            kind: AddressKind::Value,
            value,
            seq,
        }
    }

    fn usage_row(id: &str, seq: u64) -> UsageRow {
        UsageRow {
            id: UsageId::from(id.to_owned()),
            seq,
            usage: pi_ai::Usage {
                input: 3,
                ..pi_ai::Usage::default()
            },
            entry_id: Some(EntryId::from("e1".to_owned())),
            adjustment: false,
            details: None,
        }
    }

    /// One entry on branch `main`, its lane values, and one usage row.
    #[expect(clippy::expect_used, reason = "test fixture serialization cannot fail")]
    fn source_snapshot() -> ForkSourceSnapshot {
        let tip = serde_json::to_value(Some(EntryId::from("e1".to_owned())))
            .expect("branch tip serialization cannot fail");
        ForkSourceSnapshot {
            entries: vec![entry("e1", 1)],
            values: vec![
                scalar("pi.branch.tip", "main", tip, 2),
                scalar("pi.lane.config", "main", serde_json::json!({}), 3),
                scalar("pi.lane.state", "main", serde_json::json!({}), 4),
            ],
            usage: vec![usage_row("u1", 5)],
            entries_complete: true,
        }
    }

    #[test]
    fn tree_fork_copies_usage_rows_verbatim() -> Result<(), SessionError> {
        let destination =
            create_fork_snapshot(&source_snapshot(), &ForkOptions::Tree { id: None })?;
        assert_eq!(destination.usage.len(), 1);
        let row = &destination.usage[0];
        assert_eq!(row.id.as_str(), "u1");
        assert_eq!(row.usage.input, 3);
        assert_eq!(row.entry_id.as_ref().map(EntryId::as_str), Some("e1"));
        // The row keeps its source sequence (5), which sits above the largest
        // copied entry (1) — reconstructed values must start above it instead
        // of colliding with it on replay.
        assert_eq!(row.seq, 5);
        assert!(
            !destination
                .values
                .iter()
                .any(|stored| stored.seq == row.seq)
        );
        let max_value_seq = destination
            .values
            .iter()
            .map(|stored| stored.seq)
            .max()
            .unwrap_or_default();
        assert!(row.seq < max_value_seq);
        assert_eq!(max_value_seq + 1, destination.next_seq);
        let writes = fork_snapshot_writes(&destination);
        assert!(
            writes.iter().any(
                |write| matches!(write, CommittedWrite::Usage(row) if row.id.as_str() == "u1")
            )
        );
        Ok(())
    }

    #[test]
    fn branch_fork_drops_usage_rows() -> Result<(), SessionError> {
        let destination = create_fork_snapshot(
            &source_snapshot(),
            &ForkOptions::Branch {
                branch: LaneName::from("main"),
                entry_id: None,
                position: ForkPosition::At,
                id: None,
            },
        )?;
        assert!(destination.usage.is_empty());
        let writes = fork_snapshot_writes(&destination);
        assert!(
            writes
                .iter()
                .all(|write| !matches!(write, CommittedWrite::Usage(_)))
        );
        Ok(())
    }
}
