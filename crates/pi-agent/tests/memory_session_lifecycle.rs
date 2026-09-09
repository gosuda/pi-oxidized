//! Behavioral regressions for the memory session repository and shared session.
#![expect(
    clippy::expect_used,
    reason = "test assertions use expect for concise failure"
)]
#![expect(clippy::panic, reason = "test failure paths panic with context")]

use std::sync::Arc;
use std::task::{Context as TaskContext, Poll};

use futures::task::noop_waker_ref;

use pi_agent::context::Context;
use pi_agent::session::{
    Branch, EntryId, EntryQuery, ForkOptions, ForkPosition, IdGenerator, InboxItem, InboxItemKind,
    LaneConfiguration, LaneName, LaneState, ListReadOptions, MemoryCreateOptions,
    MemorySessionRepo, MemoryStorage, ModelIdentity, NewUsageRow, OperationId, RawAddress, Session,
    SessionError, SessionMetadata, SessionReaderExt, SessionRepo, Storage, StorageBackedSession,
    StorageErrorCode, UsageId, UuidV7Generator, ValueList, ValueWrite, Write, address, append_list,
    set_value,
};
use serde_json::json;

fn metadata(id: &str) -> SessionMetadata {
    SessionMetadata {
        id: id.to_owned(),
        created_at: 1,
        storage_version: MemoryStorage::STORAGE_VERSION,
        cwd: None,
        parent_session_id: None,
        legacy_parent_session_path: None,
    }
}

async fn create_session(repo: &MemorySessionRepo, id: &str, cx: &Context) -> Arc<dyn Session> {
    repo.create(
        MemoryCreateOptions {
            metadata: Some(metadata(id)),
        },
        cx,
    )
    .await
    .expect("session creation should succeed")
}

fn require_branch(handle: Option<Arc<dyn Branch>>, what: &'static str) -> Arc<dyn Branch> {
    handle.unwrap_or_else(|| panic!("{what}"))
}

fn assert_closed<T>(result: Result<T, SessionError>) {
    assert!(
        matches!(result, Err(SessionError::Backend(failure)) if failure.code == StorageErrorCode::Closed),
        "expected closed error"
    );
}

fn assert_corrupt<T>(result: Result<T, SessionError>) {
    assert!(
        matches!(result, Err(SessionError::Backend(failure)) if failure.code == StorageErrorCode::Corrupt),
        "expected corrupt error"
    );
}

struct ForkFixture {
    metadata: SessionMetadata,
    lane: LaneName,
    root: EntryId,
    child: EntryId,
    lane_config: LaneConfiguration,
    ordinary_list: RawAddress,
    pending_list: RawAddress,
    operation_value: RawAddress,
    result_value: RawAddress,
    pending_value: RawAddress,
}

#[allow(clippy::too_many_lines)]
async fn seed_fork_fixture(repo: &MemorySessionRepo, cx: &Context) -> ForkFixture {
    let session = create_session(repo, "source", cx).await;
    let metadata = session.metadata().clone();
    let lane = LaneName::from("main");
    let branch = session
        .create_branch(&lane, None, cx)
        .await
        .expect("main branch should exist");
    let root = branch
        .append_custom_entry("note", Some(json!({"at": "root"})), cx)
        .await
        .expect("root append should succeed");
    let child = branch
        .append_custom_entry("note", Some(json!({"at": "child"})), cx)
        .await
        .expect("child append should succeed");

    let lane_config = LaneConfiguration {
        model: ModelIdentity {
            provider: "provider".to_owned(),
            model_id: "model".to_owned(),
            api: None,
        },
        thinking_level: pi_agent::pi_ai::ModelThinkingLevel::Off,
        active_tool_names: vec!["tool".to_owned()],
    };
    let lane_state = LaneState {
        current_operation_id: Some(OperationId::from("op-current")),
        last_operation_id: Some(OperationId::from("op-last")),
        inbox: vec![InboxItem {
            entry_id: child.clone(),
            kind: InboxItemKind::FollowUp,
        }],
    };

    let ordinary_list = ValueList::<serde_json::Value>::new("app.list", "events")
        .expect("test list address should be valid")
        .erase();
    let pending_list =
        ValueList::<serde_json::Value>::new("pi.pending.assistant_frame", "op-1:response")
            .expect("test pending list address should be valid")
            .erase();
    let operation_value = RawAddress::value("pi.op.state", "op-1");
    let result_value = RawAddress::value("pi.result", "op-1");
    let pending_value = RawAddress::value("pi.pending.entry", "entry-1");

    let usage = pi_agent::pi_ai::Usage {
        total_tokens: 7,
        ..pi_agent::pi_ai::Usage::default()
    };
    let mutation = session
        .begin_mutation(cx)
        .await
        .expect("mutation should open");
    let writes = vec![
        set_value(&address::lane_config(&lane), &lane_config)
            .expect("lane config should serialize"),
        set_value(&address::lane_state(&lane), &lane_state).expect("lane state should serialize"),
        append_list(
            &ValueList::<serde_json::Value>::new("app.list", "events").expect("list address"),
            &json!("ordinary"),
        )
        .expect("ordinary list should serialize"),
        append_list(
            &ValueList::<serde_json::Value>::new("pi.pending.assistant_frame", "op-1:response")
                .expect("pending list address"),
            &json!("transient"),
        )
        .expect("pending list should serialize"),
        Write::Value(ValueWrite::Set {
            namespace: operation_value.namespace.clone(),
            key: operation_value.key.clone(),
            value: json!({"state": "running"}),
        }),
        Write::Value(ValueWrite::Set {
            namespace: result_value.namespace.clone(),
            key: result_value.key.clone(),
            value: json!({"status": "completed"}),
        }),
        Write::Value(ValueWrite::Set {
            namespace: pending_value.namespace.clone(),
            key: pending_value.key.clone(),
            value: json!({"payload": "staged"}),
        }),
        Write::Usage {
            row: NewUsageRow {
                id: UsageId::from("usage-1"),
                usage,
                entry_id: Some(child.clone()),
                adjustment: false,
                details: None,
            },
        },
    ];
    mutation
        .commit(writes, cx)
        .await
        .expect("fixture writes should commit");

    ForkFixture {
        metadata,
        lane,
        root,
        child,
        lane_config,
        ordinary_list,
        pending_list,
        operation_value,
        result_value,
        pending_value,
    }
}

#[tokio::test]
async fn close_then_reopen_preserves_durable_branches_and_entries() {
    let repo = MemorySessionRepo::new();
    let cx = Context::background();
    let session = create_session(&repo, "durable", &cx).await;
    let metadata = session.metadata().clone();
    let main = LaneName::from("main");
    let empty = LaneName::from("empty");
    let main_branch = session
        .create_branch(&main, None, &cx)
        .await
        .expect("main branch should exist");
    session
        .create_branch(&empty, None, &cx)
        .await
        .expect("empty branch should exist");
    let root = main_branch
        .append_custom_entry("note", Some(json!({"body": "root"})), &cx)
        .await
        .expect("entry should append");
    let child = main_branch
        .append_custom_entry("note", Some(json!({"body": "child"})), &cx)
        .await
        .expect("entry should append");
    session.close(&cx).await.expect("close should complete");

    let reopened = repo
        .open(&metadata, &cx)
        .await
        .expect("closed session should reopen");
    let reopened_main = require_branch(
        reopened
            .branch(&main, &cx)
            .await
            .expect("branch lookup should succeed"),
        "durable main branch should remain visible",
    );
    assert_eq!(
        reopened_main
            .get_tip_id(&cx)
            .await
            .expect("tip should decode"),
        Some(child.clone())
    );
    let entries = reopened_main
        .find_entries(
            Some(&pi_agent::session::BranchScan {
                order: Some(pi_agent::session::ScanOrder::Asc),
                ..Default::default()
            }),
            &cx,
        )
        .await
        .expect("durable ancestry should be readable");
    assert_eq!(
        entries
            .iter()
            .map(|entry| entry.id().clone())
            .collect::<Vec<_>>(),
        vec![root, child]
    );
    let reopened_empty = require_branch(
        reopened
            .branch(&empty, &cx)
            .await
            .expect("empty branch lookup should succeed"),
        "empty branch existence should be durable",
    );
    assert_eq!(
        reopened_empty
            .get_tip_id(&cx)
            .await
            .expect("empty tip should decode"),
        None
    );
}

#[tokio::test]
async fn open_rejects_double_open_and_delete_while_open() {
    let repo = MemorySessionRepo::new();
    let cx = Context::background();
    let session = create_session(&repo, "lifecycle", &cx).await;
    let metadata = session.metadata().clone();

    match repo.open(&metadata, &cx).await {
        Err(SessionError::Invariant(message)) => {
            assert_eq!(message, "session is already open: lifecycle");
        }
        other => panic!(
            "expected double-open rejection, got {}",
            if other.is_ok() {
                "Ok"
            } else {
                "different error"
            }
        ),
    }
    match repo.delete(&metadata, &cx).await {
        Err(SessionError::Invariant(message)) => assert_eq!(message, "session is open: lifecycle"),
        other => panic!(
            "expected delete-while-open rejection, got {}",
            if other.is_ok() {
                "Ok"
            } else {
                "different error"
            }
        ),
    }

    session.close(&cx).await.expect("close should complete");
    repo.delete(&metadata, &cx)
        .await
        .expect("closed session should be deletable");
}

#[tokio::test]
async fn retained_branch_rejects_writes_after_owning_session_closes() {
    let repo = MemorySessionRepo::new();
    let cx = Context::background();
    let session = create_session(&repo, "retained-branch", &cx).await;
    let lane = LaneName::from("main");
    let branch = session
        .create_branch(&lane, None, &cx)
        .await
        .expect("branch should exist");
    session.close(&cx).await.expect("close should complete");

    assert_closed(branch.append_custom_entry("note", None, &cx).await);
    assert_closed(branch.get_tip_id(&cx).await);
}

#[tokio::test]
async fn malformed_stored_branch_tip_returns_corrupt_instead_of_new_root() {
    let cx = Context::background();
    let storage = Arc::new(MemoryStorage::new());
    storage
        .commit(
            vec![Write::Value(ValueWrite::Set {
                namespace: "pi.branch.tip".to_owned(),
                key: "main".to_owned(),
                value: json!(17),
            })],
            &cx,
        )
        .await
        .expect("malformed value should still be storable as raw JSON");
    let id_generator: Arc<dyn IdGenerator> = Arc::new(UuidV7Generator::new());
    let session = StorageBackedSession::new(metadata("corrupt-tip"), storage, id_generator, None);
    let lane = LaneName::from("main");

    assert_corrupt(session.branch(&lane, &cx).await);
}

#[tokio::test]
async fn branch_fork_rejects_an_unconfigured_lane() {
    let repo = MemorySessionRepo::new();
    let cx = Context::background();
    let source = create_session(&repo, "unconfigured-source", &cx).await;
    let metadata = source.metadata().clone();
    let lane = LaneName::from("unconfigured");
    source
        .create_branch(&lane, None, &cx)
        .await
        .expect("branch should exist");

    match repo
        .fork(
            &metadata,
            ForkOptions::Branch {
                branch: lane,
                entry_id: None,
                position: ForkPosition::At,
                id: Some("unconfigured-fork".to_owned()),
            },
            &cx,
        )
        .await
    {
        Err(SessionError::Invariant(message)) => {
            assert_eq!(
                message,
                "source branch unconfigured is not a configured AgentLane"
            );
        }
        Err(error) => panic!("expected unconfigured-lane invariant, got {error:?}"),
        Ok(_) => panic!("unconfigured branch fork unexpectedly succeeded"),
    }
}

#[tokio::test]
#[allow(clippy::too_many_lines)]
async fn branch_forks_at_root_before_and_at_preserve_the_expected_ancestry() {
    let repo = MemorySessionRepo::new();
    let cx = Context::background();
    let fixture = seed_fork_fixture(&repo, &cx).await;

    let before = repo
        .fork(
            &fixture.metadata,
            ForkOptions::Branch {
                branch: fixture.lane.clone(),
                entry_id: Some(fixture.root.clone()),
                position: ForkPosition::Before,
                id: Some("before-root".to_owned()),
            },
            &cx,
        )
        .await
        .expect("before-root fork should succeed");
    let before_branch = require_branch(
        before
            .branch(&fixture.lane, &cx)
            .await
            .expect("before-root branch lookup should succeed"),
        "before-root branch should exist",
    );
    assert_eq!(
        before_branch
            .get_tip_id(&cx)
            .await
            .expect("before-root tip should decode"),
        None
    );
    assert!(
        before_branch
            .find_entries(None, &cx)
            .await
            .expect("before-root ancestry should read")
            .is_empty()
    );
    assert_eq!(
        before
            .get_value(&address::lane_config(&fixture.lane), &cx)
            .await
            .expect("before-root config should read")
            .expect("before-root config should be retained")
            .value,
        fixture.lane_config
    );
    assert_eq!(
        before
            .get_value(&address::lane_state(&fixture.lane), &cx)
            .await
            .expect("before-root state should read")
            .expect("before-root state should be rebuilt")
            .value,
        LaneState::default()
    );
    for address in [
        &fixture.operation_value,
        &fixture.result_value,
        &fixture.pending_value,
    ] {
        assert!(
            before
                .get_value_json(address, &cx)
                .await
                .expect("before-root transient value read should succeed")
                .is_none()
        );
    }
    for address in [&fixture.ordinary_list, &fixture.pending_list] {
        assert!(
            before
                .read_list_json(address, Some(ListReadOptions::default()), &cx)
                .await
                .expect("before-root list read should succeed")
                .is_empty()
        );
    }
    assert_eq!(
        before
            .get_stats(&cx)
            .await
            .expect("before-root stats should read")
            .usage,
        pi_agent::pi_ai::Usage::default()
    );

    let at = repo
        .fork(
            &fixture.metadata,
            ForkOptions::Branch {
                branch: fixture.lane.clone(),
                entry_id: Some(fixture.root.clone()),
                position: ForkPosition::At,
                id: Some("at-root".to_owned()),
            },
            &cx,
        )
        .await
        .expect("at-root fork should succeed");
    let at_branch = require_branch(
        at.branch(&fixture.lane, &cx)
            .await
            .expect("at-root branch lookup should succeed"),
        "at-root branch should exist",
    );
    assert_eq!(
        at_branch
            .get_tip_id(&cx)
            .await
            .expect("at-root tip should decode"),
        Some(fixture.root.clone())
    );
    let entries = at_branch
        .find_entries(
            Some(&pi_agent::session::BranchScan {
                order: Some(pi_agent::session::ScanOrder::Asc),
                ..Default::default()
            }),
            &cx,
        )
        .await
        .expect("at-root ancestry should read");
    assert_eq!(
        entries
            .iter()
            .map(|entry| entry.id().clone())
            .collect::<Vec<_>>(),
        vec![fixture.root]
    );
    assert!(
        at.get_entry(&fixture.child, &cx)
            .await
            .expect("child lookup should succeed")
            .is_none()
    );
}

#[tokio::test]
async fn tree_fork_rebuilds_lane_state_and_excludes_transient_values_lists_and_usage() {
    let repo = MemorySessionRepo::new();
    let cx = Context::background();
    let fixture = seed_fork_fixture(&repo, &cx).await;
    let tree = repo
        .fork(
            &fixture.metadata,
            ForkOptions::Tree {
                id: Some("tree".to_owned()),
            },
            &cx,
        )
        .await
        .expect("tree fork should succeed");

    let entries = tree
        .find_entries(
            Some(&EntryQuery {
                order: Some(pi_agent::session::ScanOrder::Asc),
                ..Default::default()
            }),
            &cx,
        )
        .await
        .expect("tree entries should read");
    assert_eq!(
        entries
            .iter()
            .map(|entry| entry.id().clone())
            .collect::<Vec<_>>(),
        vec![fixture.root.clone(), fixture.child.clone()]
    );
    let tree_branch = require_branch(
        tree.branch(&fixture.lane, &cx)
            .await
            .expect("tree branch lookup should succeed"),
        "tree branch should exist",
    );
    assert_eq!(
        tree_branch
            .get_tip_id(&cx)
            .await
            .expect("tree tip should decode"),
        Some(fixture.child)
    );
    assert_eq!(
        tree.get_value(&address::lane_state(&fixture.lane), &cx)
            .await
            .expect("tree state should read")
            .expect("tree state should be rebuilt")
            .value,
        LaneState::default()
    );
    assert_eq!(
        tree.get_value(&address::lane_config(&fixture.lane), &cx)
            .await
            .expect("tree config should read")
            .expect("tree config should be retained")
            .value,
        fixture.lane_config
    );
    for address in [
        &fixture.operation_value,
        &fixture.result_value,
        &fixture.pending_value,
    ] {
        assert!(
            tree.get_value_json(address, &cx)
                .await
                .expect("transient value read should succeed")
                .is_none()
        );
    }
    for address in [&fixture.ordinary_list, &fixture.pending_list] {
        assert!(
            tree.read_list_json(address, Some(ListReadOptions::default()), &cx)
                .await
                .expect("forked list read should succeed")
                .is_empty()
        );
    }
    let stats = tree.get_stats(&cx).await.expect("tree stats should read");
    assert_eq!(stats.usage, pi_agent::pi_ai::Usage::default());
}

#[tokio::test(flavor = "current_thread")]
async fn dropping_close_waiter_drains_admitted_mutation_and_allows_reopen() {
    let repo = MemorySessionRepo::new();
    let cx = Context::background();
    let session = create_session(&repo, "drop-close-waiter", &cx).await;
    let metadata = session.metadata().clone();
    let mutation = session
        .begin_mutation(&cx)
        .await
        .expect("mutation should be admitted");

    let mut close_waiter = session.close(&cx);
    let mut poll_cx = TaskContext::from_waker(noop_waker_ref());
    assert!(matches!(
        close_waiter.as_mut().poll(&mut poll_cx),
        Poll::Pending
    ));
    drop(close_waiter);

    // Release the admitted mutation; the close worker continues without a
    // second close caller and the repo's on_close flips record.open.
    drop(mutation);

    let mut reopened = None;
    for _ in 0..100 {
        match repo.open(&metadata, &cx).await {
            Ok(session) => {
                reopened = Some(session);
                break;
            }
            Err(SessionError::Invariant(message))
                if message == "session is already open: drop-close-waiter" =>
            {
                tokio::task::yield_now().await;
            }
            Err(error) => panic!("unexpected reopen result: {error:?}"),
        }
    }
    let reopened = reopened.expect("repository should reopen once the close callback finishes");
    assert_eq!(reopened.metadata().id, metadata.id);
}
