//! Regression tests for parent validation in `validate_committed_writes`.
//!
//! An entry parent must name a committed *entry* or an entry written earlier
//! in the same batch. Usage-row ids share the global duplicate-id namespace
//! but can never parent an entry, and an entry can never parent itself.
#![expect(clippy::panic, reason = "test failure paths panic with context")]
#![expect(
    clippy::expect_used,
    reason = "test assertions use expect for concise failure"
)]

use std::collections::HashSet;

use pi_agent::AgentMessage;
use pi_agent::context::Context;
use pi_agent::pi_ai;
use pi_agent::session::{
    CommittedIdView, Entry, EntryId, MemoryStorage, NewEntry, NewEntryBody, NewUsageRow,
    SessionError, Storage, UsageId, Write, validate_committed_writes,
};

/// Minimal committed-state view: `ids` is the global identity namespace
/// (committed entries and usage rows), `entry_ids` is entry-only membership.
struct View {
    ids: HashSet<String>,
    entry_ids: HashSet<EntryId>,
    next_seq: u64,
}

impl View {
    fn empty() -> Self {
        Self {
            ids: HashSet::new(),
            entry_ids: HashSet::new(),
            next_seq: 1,
        }
    }

    fn committed_entry(id: &str) -> Self {
        Self {
            ids: HashSet::from([id.to_owned()]),
            entry_ids: HashSet::from([EntryId::from(id)]),
            next_seq: 1,
        }
    }

    fn committed_usage(id: &str) -> Self {
        Self {
            ids: HashSet::from([id.to_owned()]),
            entry_ids: HashSet::new(),
            next_seq: 1,
        }
    }
}

impl CommittedIdView for View {
    fn contains_id(&self, id: &str) -> bool {
        self.ids.contains(id)
    }

    fn contains_entry_id(&self, id: &EntryId) -> bool {
        self.entry_ids.contains(id)
    }

    fn next_seq(&self) -> u64 {
        self.next_seq
    }
}

fn entry(id: &str, parent: Option<&str>) -> Write {
    Write::Entry {
        entry: NewEntry {
            id: EntryId::from(id),
            parent_id: parent.map(EntryId::from),
            body: NewEntryBody::Custom {
                custom_type: "note".to_owned(),
                data: None,
            },
        },
    }
}

fn usage(id: &str) -> Write {
    Write::Usage {
        row: NewUsageRow {
            id: UsageId::from(id),
            usage: pi_ai::Usage::default(),
            entry_id: None,
            adjustment: false,
            details: None,
        },
    }
}

fn pending_assistant_entry(id: &str, parent: Option<&str>) -> Write {
    let mut assistant = pi_ai::AssistantMessage::new("openai-completions", "openai", "model", 0);
    assistant.stop_reason = pi_ai::StopReason::Pending;
    Write::Entry {
        entry: NewEntry {
            id: EntryId::from(id),
            parent_id: parent.map(EntryId::from),
            body: NewEntryBody::Message {
                message: AgentMessage::Llm(Box::new(pi_ai::Message::Assistant(Box::new(
                    assistant,
                )))),
                terminate: false,
            },
        },
    }
}

fn expect_missing_parent<T: std::fmt::Debug>(result: Result<T, SessionError>, parent: &str) {
    match result {
        Err(SessionError::Invariant(message)) => {
            assert_eq!(message, format!("missing parent id {parent}"));
        }
        other => panic!("expected missing parent id {parent}, got {other:?}"),
    }
}

#[test]
fn accepts_committed_entry_parent() {
    let view = View::committed_entry("root");
    assert!(validate_committed_writes(&[entry("child", Some("root"))], 1, &view).is_ok());
}

#[test]
fn accepts_earlier_same_batch_entry_parent() {
    let view = View::empty();
    let writes = [
        entry("a", None),
        entry("b", Some("a")),
        entry("c", Some("b")),
    ];
    assert!(validate_committed_writes(&writes, 1, &view).is_ok());
}

#[test]
fn accepts_mixed_batch_with_entry_chain() {
    let view = View::committed_entry("root");
    let writes = [entry("a", Some("root")), usage("u1"), entry("b", Some("a"))];
    assert!(validate_committed_writes(&writes, 1, &view).is_ok());
}

#[test]
fn rejects_self_parent() {
    let view = View::empty();
    expect_missing_parent(
        validate_committed_writes(&[entry("a", Some("a"))], 1, &view),
        "a",
    );
}

#[test]
fn rejects_committed_usage_row_as_parent() {
    let view = View::committed_usage("u1");
    expect_missing_parent(
        validate_committed_writes(&[entry("a", Some("u1"))], 1, &view),
        "u1",
    );
}

#[test]
fn rejects_earlier_same_batch_usage_row_as_parent() {
    let view = View::empty();
    let writes = [usage("u1"), entry("a", Some("u1"))];
    expect_missing_parent(validate_committed_writes(&writes, 1, &view), "u1");
}

#[test]
fn rejects_later_same_batch_entry_as_parent() {
    let view = View::empty();
    let writes = [entry("b", Some("a")), entry("a", None)];
    expect_missing_parent(validate_committed_writes(&writes, 1, &view), "a");
}

#[test]
fn rejects_unknown_parent() {
    let view = View::empty();
    expect_missing_parent(
        validate_committed_writes(&[entry("a", Some("ghost"))], 1, &view),
        "ghost",
    );
}

#[test]
fn rejects_cross_kind_duplicate_ids() {
    let view = View::empty();
    match validate_committed_writes(&[entry("x", None), usage("x")], 1, &view) {
        Err(SessionError::Invariant(message)) => assert_eq!(message, "duplicate usage id x"),
        other => panic!("expected duplicate usage id, got {other:?}"),
    }
    match validate_committed_writes(&[usage("x"), entry("x", None)], 1, &view) {
        Err(SessionError::Invariant(message)) => assert_eq!(message, "duplicate entry id x"),
        other => panic!("expected duplicate entry id, got {other:?}"),
    }
}

#[test]
fn rejects_id_committed_under_other_kind() {
    let view = View::committed_usage("u1");
    match validate_committed_writes(&[entry("u1", None)], 1, &view) {
        Err(SessionError::Invariant(message)) => assert_eq!(message, "duplicate entry id u1"),
        other => panic!("expected duplicate entry id, got {other:?}"),
    }
    let view = View::committed_entry("e1");
    match validate_committed_writes(&[usage("e1")], 1, &view) {
        Err(SessionError::Invariant(message)) => assert_eq!(message, "duplicate usage id e1"),
        other => panic!("expected duplicate usage id, got {other:?}"),
    }
}

#[test]
fn rejects_pending_assistant_message() {
    let view = View::empty();
    let result = validate_committed_writes(&[pending_assistant_entry("a", None)], 1, &view);
    assert!(
        matches!(result, Err(SessionError::PendingAssistantMessage)),
        "got {result:?}"
    );
}

#[test]
fn rejects_sequence_mismatch() {
    let view = View::empty();
    let result = validate_committed_writes(&[entry("a", None)], 2, &view);
    match result {
        Err(SessionError::Invariant(message)) => {
            assert_eq!(message, "commit starts at sequence 2, expected 1");
        }
        other => panic!("expected sequence invariant, got {other:?}"),
    }
}

#[tokio::test]
async fn storage_accepts_committed_and_earlier_entry_parents() {
    let storage = MemoryStorage::new();
    let cx = Context::background();

    storage
        .commit(vec![entry("root", None)], &cx)
        .await
        .expect("root commit should succeed");
    let result = storage
        .commit(
            vec![
                entry("child", Some("root")),
                entry("grandchild", Some("child")),
            ],
            &cx,
        )
        .await
        .expect("true entry ancestors should be accepted");

    assert_eq!(result.seqs, vec![2, 3]);
    let root_id = EntryId::from("root");
    let child_id = EntryId::from("child");
    let grandchild_id = EntryId::from("grandchild");
    let entries = storage
        .get_entries(
            &[root_id.clone(), child_id.clone(), grandchild_id.clone()],
            &cx,
        )
        .await
        .expect("committed entries should be readable");
    assert_eq!(
        entries.get(&child_id).and_then(Entry::parent_id),
        Some(&root_id)
    );
    assert_eq!(
        entries.get(&grandchild_id).and_then(Entry::parent_id),
        Some(&child_id)
    );
}

#[tokio::test]
async fn storage_rejects_usage_parent_without_partial_commit() {
    let storage = MemoryStorage::new();
    let cx = Context::background();

    storage
        .commit(vec![usage("u1")], &cx)
        .await
        .expect("usage seed should succeed");
    let result = storage
        .commit(vec![entry("prefix", None), entry("child", Some("u1"))], &cx)
        .await;
    expect_missing_parent(result, "u1");

    let prefix_id = EntryId::from("prefix");
    let child_id = EntryId::from("child");
    let entries = storage
        .get_entries(&[prefix_id.clone(), child_id], &cx)
        .await
        .expect("failed batch entries should be readable");
    assert!(
        entries.is_empty(),
        "failed batch must not expose a committed prefix"
    );

    let stats = storage
        .get_stats(&cx)
        .await
        .expect("stats should be readable");
    assert_eq!(stats.message_count, 0);
    assert_eq!(stats.usage, pi_ai::Usage::default());

    let retry = storage
        .commit(vec![entry("prefix", None)], &cx)
        .await
        .expect("uncommitted prefix should be retryable");
    assert_eq!(retry.first_seq, 2);
}
