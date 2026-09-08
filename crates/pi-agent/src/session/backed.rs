//! Backend-neutral session implementation over [`Storage`].
//!
//! [`StorageBackedSession`] is the one session implementation every backend
//! shares: branch existence and tips live in durable `pi.branch.tip` values,
//! mutation is serialized by a single lock, and every derived handle observes
//! the session's shared closed state.

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex as StdMutex, OnceLock, Weak};

use futures::future::BoxFuture;
use tokio::sync::{watch, Mutex, OwnedMutexGuard};

use crate::context::Context;
use crate::message::AgentMessage;
use super::address::{self, ListReadOptions, RawAddress};
use super::entry::{Entry, NewEntry, NewEntryBody};
use super::error::{SessionError, StorageErrorCode, StorageFailure};
use super::ids::{EntryId, LaneName};
use super::scan::{BranchScan, EntryQuery, EntryScan, RawListElement, RawStoredValue, ScanOrder, StorageBranchScan};
use super::traits::{Branch, IdGenerator, MutationGuard, Session, SessionMetadata, SessionMutation, SessionReader, SessionReaderExt, Storage};
use super::write::{self, CommitResult, ListWrite, SessionStats, ValueWrite, Write};

type CloseResult = Result<(), SessionError>;
type CloseCallback = Box<dyn Fn() + Send + Sync>;
type CloseHook = Arc<StdMutex<Option<CloseCallback>>>;

struct CloseOperation {
    result: watch::Receiver<Option<CloseResult>>,
}

impl CloseOperation {
    async fn wait(&self) -> CloseResult {
        let mut result = self.result.clone();
        loop {
            if let Some(outcome) = result.borrow().as_ref().cloned() {
                return outcome;
            }
            result.changed().await.map_err(|_| SessionError::Invariant("session close task ended before publishing result".to_owned()))?;
        }
    }
}

struct CloseState {
    storage: Arc<dyn Storage>,
    mutation: Arc<Mutex<()>>,
    on_close: CloseHook,
    operation: OnceLock<Arc<CloseOperation>>,
}

impl CloseState {
    fn new(
        storage: Arc<dyn Storage>,
        mutation: Arc<Mutex<()>>,
        on_close: Option<CloseCallback>,
    ) -> Self {
        Self {
            storage,
            mutation,
            on_close: Arc::new(StdMutex::new(on_close)),
            operation: OnceLock::new(),
        }
    }

    fn start(&self, cx: &Context) -> Arc<CloseOperation> {
        self.operation.get_or_init(|| {
            let (sender, receiver) = watch::channel::<Option<CloseResult>>(None);
            let storage = Arc::clone(&self.storage);
            let mutation = Arc::clone(&self.mutation);
            let on_close = Arc::clone(&self.on_close);
            let context = cx.clone();
            tokio::spawn(async move {
                let guard = mutation.lock().await;
                let close_context = context.without_cancellation();
                let result = storage.close(&close_context).await;
                drop(guard);

                let callback = match on_close.lock() {
                    Ok(mut on_close) => on_close.take(),
                    Err(poisoned) => poisoned.into_inner().take(),
                };
                if let Some(callback) = callback {
                    callback();
                }
                let _ = sender.send(Some(result));
            });
            Arc::new(CloseOperation { result: receiver })
        }).clone()
    }
}

/// The session is closed; returned by every operation admitted after
/// [`Session::close`] begins.
pub(super) fn closed_error() -> SessionError {
    SessionError::Backend(StorageFailure::new(StorageErrorCode::Closed, "session storage is closed"))
}

/// The caller's cancellation token fired before the operation touched state.
pub(super) fn aborted_error() -> SessionError {
    SessionError::Backend(StorageFailure::new(StorageErrorCode::Aborted, "operation cancelled"))
}

/// Rejects branch names a backend could not key on: empty, or containing NUL.
pub(super) fn validate_branch_name(name: &LaneName) -> Result<(), SessionError> {
    if name.as_str().is_empty() {
        return Err(SessionError::InvalidBranch { branch: name.clone(), reason: "branch name must not be empty".to_owned() });
    }
    if name.as_str().contains('\0') {
        return Err(SessionError::InvalidBranch { branch: name.clone(), reason: "branch name must not contain \\u0000".to_owned() });
    }
    Ok(())
}

/// A [`Session`] over any [`Storage`] backend.
///
/// Branch existence is the presence of a durable `pi.branch.tip` value — there
/// is no implicit `main` lane and no in-memory branch registry, so a reopened
/// session sees exactly the branches its backend persisted. A stored `null`
/// tip is an existing but empty branch. All handles derived from one session
/// (mutation guards, branch handles) observe the same `closed` flag, so
/// closing the session rejects every later operation regardless of which
/// handle issues it.
///
/// Construct through [`StorageBackedSession::new`], which returns `Arc<Self>`
/// because branch handles back-reference the session.
pub struct StorageBackedSession {
    metadata: SessionMetadata,
    storage: Arc<dyn Storage>,
    id_generator: Arc<dyn IdGenerator>,
    mutation: Arc<Mutex<()>>,
    closed: AtomicBool,
    close_state: Arc<CloseState>,
    this: Weak<StorageBackedSession>,
}

impl StorageBackedSession {
    /// Opens a session over `storage`.
    ///
    /// `on_close` runs exactly once when the session's close sequence
    /// completes — repositories use it to release their open-session
    /// reservation. It fires even when the backend's `close` reports an error,
    /// matching the source's `finally` semantics.
    #[must_use]
    pub fn new(
        metadata: SessionMetadata,
        storage: Arc<dyn Storage>,
        id_generator: Arc<dyn IdGenerator>,
        on_close: Option<Box<dyn Fn() + Send + Sync>>,
    ) -> Arc<Self> {
        let mutation = Arc::new(Mutex::new(()));
        let close_state = Arc::new(CloseState::new(Arc::clone(&storage), Arc::clone(&mutation), on_close));
        Arc::new_cyclic(|this| Self {
            metadata,
            storage,
            id_generator,
            mutation,
            closed: AtomicBool::new(false),
            close_state,
            this: this.clone(),
        })
    }

    fn ensure_open(&self) -> Result<(), SessionError> {
        if self.closed.load(Ordering::Acquire) { Err(closed_error()) } else { Ok(()) }
    }

    fn branch_handle(&self, name: &LaneName) -> Result<Arc<dyn Branch>, SessionError> {
        let session = self.this.upgrade().ok_or_else(|| SessionError::Invariant("session handle dropped".to_owned()))?;
        Ok(Arc::new(StorageBackedBranch { session, name: name.clone() }))
    }

    /// Appends `body` as a new entry on `name`'s tip and moves the tip in the
    /// same commit.
    ///
    /// The pending-assistant rejection runs before the id is reserved so a
    /// refused append never burns a generator id, and before the mutation slot
    /// is taken so it never waits on the line.
    async fn append_to_branch(&self, name: &LaneName, body: NewEntryBody, cx: &Context) -> Result<EntryId, SessionError> {
        self.ensure_open()?;
        if let NewEntryBody::Message { message: AgentMessage::Llm(llm), .. } = &body
            && let pi_ai::Message::Assistant(assistant) = llm.as_ref()
            && matches!(assistant.stop_reason, pi_ai::StopReason::Pending)
        {
            return Err(SessionError::PendingAssistantMessage);
        }
        let id = EntryId::from(self.id_generator.next(None)?);
        let tip_address = address::branch_tip(name.as_str());
        let mutation = self.begin_mutation(cx).await?;
        let parent_id = mutation
            .get_value(&tip_address, cx)
            .await?
            .ok_or_else(|| SessionError::Invariant(format!("unknown branch: {name}")))?
            .value;
        mutation
            .commit(
                vec![
                    Write::Entry { entry: NewEntry { id: id.clone(), parent_id, body } },
                    write::set_value(&tip_address, &Some(id.clone()))?,
                ],
                cx,
            )
            .await?;
        Ok(id)
    }
}

impl SessionReader for StorageBackedSession {
    fn get_entries<'a>(&'a self, ids: &'a [EntryId], cx: &'a Context) -> BoxFuture<'a, Result<HashMap<EntryId, Entry>, SessionError>> {
        Box::pin(async move {
            cx.check().map_err(|_| aborted_error())?;
            self.ensure_open()?;
            self.storage.get_entries(ids, cx).await
        })
    }
    fn get_stats<'a>(&'a self, cx: &'a Context) -> BoxFuture<'a, Result<SessionStats, SessionError>> {
        Box::pin(async move {
            cx.check().map_err(|_| aborted_error())?;
            self.ensure_open()?;
            self.storage.get_stats(cx).await
        })
    }
    fn get_value_json<'a>(&'a self, address: &'a RawAddress, cx: &'a Context) -> BoxFuture<'a, Result<Option<RawStoredValue>, SessionError>> {
        Box::pin(async move {
            cx.check().map_err(|_| aborted_error())?;
            self.ensure_open()?;
            self.storage.get_value(address, cx).await
        })
    }
    fn scan_values_json<'a>(&'a self, prefix: &'a RawAddress, cx: &'a Context) -> BoxFuture<'a, Result<Vec<RawStoredValue>, SessionError>> {
        Box::pin(async move {
            cx.check().map_err(|_| aborted_error())?;
            self.ensure_open()?;
            self.storage.scan_values(prefix, cx).await
        })
    }
    fn read_list_json<'a>(&'a self, address: &'a RawAddress, options: Option<ListReadOptions>, cx: &'a Context) -> BoxFuture<'a, Result<Vec<RawListElement>, SessionError>> {
        Box::pin(async move {
            cx.check().map_err(|_| aborted_error())?;
            self.ensure_open()?;
            self.storage.read_list(address, options, cx).await
        })
    }
    fn scan_branch<'a>(&'a self, query: &'a StorageBranchScan, cx: &'a Context) -> BoxFuture<'a, Result<Vec<Entry>, SessionError>> {
        Box::pin(async move {
            cx.check().map_err(|_| aborted_error())?;
            self.ensure_open()?;
            self.storage.scan_branch(query, cx).await
        })
    }
}

impl Session for StorageBackedSession {
    fn metadata(&self) -> &SessionMetadata {
        &self.metadata
    }
    fn id_generator(&self) -> &dyn IdGenerator {
        &*self.id_generator
    }
    fn get_entry<'a>(&'a self, id: &'a EntryId, cx: &'a Context) -> BoxFuture<'a, Result<Option<Entry>, SessionError>> {
        Box::pin(async move {
            let mut entries = self.get_entries(std::slice::from_ref(id), cx).await?;
            Ok(entries.remove(id))
        })
    }
    fn get_name<'a>(&'a self, cx: &'a Context) -> BoxFuture<'a, Result<Option<String>, SessionError>> {
        Box::pin(async move { Ok(self.get_value(&address::session_name(), cx).await?.map(|stored| stored.value)) })
    }
    fn get_label<'a>(&'a self, target: &'a EntryId, cx: &'a Context) -> BoxFuture<'a, Result<Option<String>, SessionError>> {
        Box::pin(async move { Ok(self.get_value(&address::entry_label(target), cx).await?.map(|stored| stored.value)) })
    }
    fn find_entries<'a>(&'a self, query: Option<&'a EntryQuery>, cx: &'a Context) -> BoxFuture<'a, Result<Vec<Entry>, SessionError>> {
        Box::pin(async move {
            cx.check().map_err(|_| aborted_error())?;
            self.ensure_open()?;
            let query = query.cloned().unwrap_or_default();
            let order = query.order.unwrap_or(ScanOrder::Desc);
            let (from_seq, to_seq) = query.cursor.map_or((None, None), |cursor| match order {
                ScanOrder::Asc => (Some(cursor.seq.saturating_add(1)), None),
                ScanOrder::Desc => (None, Some(cursor.seq.saturating_sub(1))),
            });
            let scan = EntryScan { from_seq, to_seq, order: Some(order), limit: query.limit, entry_type: query.entry_type, custom_type: query.custom_type };
            self.storage.scan_entries(&scan, cx).await
        })
    }
    fn find_entry<'a>(&'a self, query: Option<&'a EntryQuery>, cx: &'a Context) -> BoxFuture<'a, Result<Option<Entry>, SessionError>> {
        Box::pin(async move {
            let mut query = query.cloned().unwrap_or_default();
            query.limit = Some(query.limit.map_or(1, |limit| limit.min(1)));
            Ok(self.find_entries(Some(&query), cx).await?.into_iter().next())
        })
    }
    fn branch<'a>(&'a self, name: &'a LaneName, cx: &'a Context) -> BoxFuture<'a, Result<Option<Arc<dyn Branch>>, SessionError>> {
        Box::pin(async move {
            validate_branch_name(name)?;
            if self.get_value(&address::branch_tip(name.as_str()), cx).await?.is_none() {
                return Ok(None);
            }
            self.branch_handle(name).map(Some)
        })
    }
    fn create_branch<'a>(&'a self, name: &'a LaneName, at: Option<&'a EntryId>, cx: &'a Context) -> BoxFuture<'a, Result<Arc<dyn Branch>, SessionError>> {
        Box::pin(async move {
            self.ensure_open()?;
            validate_branch_name(name)?;
            let tip_address = address::branch_tip(name.as_str());
            let mutation = self.begin_mutation(cx).await?;
            if mutation.get_value(&tip_address, cx).await?.is_some() {
                return Err(SessionError::BranchExists(name.clone()));
            }
            if let Some(at) = at
                && mutation.get_entries(std::slice::from_ref(at), cx).await?.is_empty()
            {
                return Err(SessionError::UnknownTarget(at.clone()));
            }
            // A `None` anchor persists JSON `null`: the branch exists but is
            // empty, exactly like a branch whose tip was never moved.
            mutation.commit(vec![write::set_value(&tip_address, &at.cloned())?], cx).await?;
            self.branch_handle(name)
        })
    }
    fn begin_mutation<'a>(&'a self, cx: &'a Context) -> BoxFuture<'a, Result<MutationGuard<'a>, SessionError>> {
        Box::pin(async move {
            cx.check().map_err(|_| aborted_error())?;
            self.ensure_open()?;
            let guard = Arc::clone(&self.mutation).lock_owned().await;
            // A close may have been admitted while this call queued on the
            // lock; sealed waiters are rejected, never run.
            self.ensure_open()?;
            cx.check().map_err(|_| aborted_error())?;
            Ok(Box::new(StorageBackedMutation { session: self, guard }) as MutationGuard<'a>)
        })
    }
    fn set_value_json<'a>(&'a self, address: &'a RawAddress, next: serde_json::Value, cx: &'a Context) -> BoxFuture<'a, Result<(), SessionError>> {
        Box::pin(async move {
            self.begin_mutation(cx)
                .await?
                .commit(vec![Write::Value(ValueWrite::Set { namespace: address.namespace.clone(), key: address.key.clone(), value: next })], cx)
                .await
                .map(|_| ())
        })
    }
    fn delete_value_json<'a>(&'a self, address: &'a RawAddress, cx: &'a Context) -> BoxFuture<'a, Result<(), SessionError>> {
        Box::pin(async move {
            self.begin_mutation(cx)
                .await?
                .commit(vec![Write::Value(ValueWrite::Delete { namespace: address.namespace.clone(), key: address.key.clone() })], cx)
                .await
                .map(|_| ())
        })
    }
    fn append_list_json<'a>(&'a self, address: &'a RawAddress, element: serde_json::Value, cx: &'a Context) -> BoxFuture<'a, Result<(), SessionError>> {
        Box::pin(async move {
            self.begin_mutation(cx)
                .await?
                .commit(vec![Write::List(ListWrite::Append { namespace: address.namespace.clone(), key: address.key.clone(), value: element })], cx)
                .await
                .map(|_| ())
        })
    }
    fn delete_list_json<'a>(&'a self, address: &'a RawAddress, cx: &'a Context) -> BoxFuture<'a, Result<(), SessionError>> {
        Box::pin(async move {
            self.begin_mutation(cx)
                .await?
                .commit(vec![Write::List(ListWrite::Delete { namespace: address.namespace.clone(), key: address.key.clone() })], cx)
                .await
                .map(|_| ())
        })
    }
    fn set_name<'a>(&'a self, name: Option<&'a str>, cx: &'a Context) -> BoxFuture<'a, Result<(), SessionError>> {
        let address = address::session_name().erase();
        Box::pin(async move {
            match name {
                Some(name) => self.set_value_json(&address, serde_json::Value::String(name.to_owned()), cx).await,
                None => self.delete_value_json(&address, cx).await,
            }
        })
    }
    fn set_label<'a>(&'a self, target: &'a EntryId, label: Option<&'a str>, cx: &'a Context) -> BoxFuture<'a, Result<(), SessionError>> {
        let address = address::entry_label(target).erase();
        Box::pin(async move {
            match label {
                Some(label) => self.set_value_json(&address, serde_json::Value::String(label.to_owned()), cx).await,
                None => self.delete_value_json(&address, cx).await,
            }
        })
    }
    fn close<'a>(&'a self, cx: &'a Context) -> BoxFuture<'a, Result<(), SessionError>> {
        // Seal admission synchronously. The owned task below is deliberately
        // independent of the caller's waiter: dropping one close future does
        // not cancel backend draining or the repository callback.
        self.closed.store(true, Ordering::Release);
        let operation = self.close_state.start(cx);
        Box::pin(async move { operation.wait().await })
    }
}

/// The single-writer handle [`Session::begin_mutation`] grants.
///
/// Reads delegate straight to the backend rather than re-checking the
/// session's open flag: a granted mutation may keep reading and commit while a
/// close waits on the mutation lock, exactly like the source. The held guard
/// proves the backend is still open — close cannot finish until it is
/// released.
struct StorageBackedMutation<'s> {
    session: &'s StorageBackedSession,
    guard: OwnedMutexGuard<()>,
}

impl SessionReader for StorageBackedMutation<'_> {
    fn get_entries<'a>(&'a self, ids: &'a [EntryId], cx: &'a Context) -> BoxFuture<'a, Result<HashMap<EntryId, Entry>, SessionError>> {
        self.session.storage.get_entries(ids, cx)
    }
    fn get_stats<'a>(&'a self, cx: &'a Context) -> BoxFuture<'a, Result<SessionStats, SessionError>> {
        self.session.storage.get_stats(cx)
    }
    fn get_value_json<'a>(&'a self, address: &'a RawAddress, cx: &'a Context) -> BoxFuture<'a, Result<Option<RawStoredValue>, SessionError>> {
        self.session.storage.get_value(address, cx)
    }
    fn scan_values_json<'a>(&'a self, prefix: &'a RawAddress, cx: &'a Context) -> BoxFuture<'a, Result<Vec<RawStoredValue>, SessionError>> {
        self.session.storage.scan_values(prefix, cx)
    }
    fn read_list_json<'a>(&'a self, address: &'a RawAddress, options: Option<ListReadOptions>, cx: &'a Context) -> BoxFuture<'a, Result<Vec<RawListElement>, SessionError>> {
        self.session.storage.read_list(address, options, cx)
    }
    fn scan_branch<'a>(&'a self, query: &'a StorageBranchScan, cx: &'a Context) -> BoxFuture<'a, Result<Vec<Entry>, SessionError>> {
        self.session.storage.scan_branch(query, cx)
    }
}

impl<'s> SessionMutation<'s> for StorageBackedMutation<'s> {
    fn commit(self: Box<Self>, writes: Vec<Write>, cx: &Context) -> BoxFuture<'s, Result<CommitResult, SessionError>> {
        let context = cx.clone();
        Box::pin(async move {
            context.check().map_err(|_| aborted_error())?;
            let StorageBackedMutation { session, guard } = *self;
            let storage = Arc::clone(&session.storage);
            let context = context.without_cancellation();
            let task = tokio::spawn(async move {
                let result = storage.commit(writes, &context).await;
                drop(guard);
                result
            });
            match task.await {
                Ok(result) => result,
                Err(error) => Err(SessionError::Invariant(format!("session commit task failed: {error}"))),
            }
        })
    }
}

/// One named conversation lane, reading and appending through its session.
///
/// The handle is a lightweight view: the tip it reports is whatever the
/// backend currently holds for that lane, and it shares the session's closed
/// flag rather than a copy.
struct StorageBackedBranch {
    session: Arc<StorageBackedSession>,
    name: LaneName,
}

impl Branch for StorageBackedBranch {
    fn name(&self) -> &LaneName {
        &self.name
    }
    fn get_tip_id<'a>(&'a self, cx: &'a Context) -> BoxFuture<'a, Result<Option<EntryId>, SessionError>> {
        Box::pin(async move {
            self.session
                .get_value(&address::branch_tip(self.name.as_str()), cx)
                .await?
                .map(|stored| stored.value)
                .ok_or_else(|| SessionError::Invariant(format!("unknown branch: {}", self.name)))
        })
    }
    fn find_entries<'a>(&'a self, query: Option<&'a BranchScan>, cx: &'a Context) -> BoxFuture<'a, Result<Vec<Entry>, SessionError>> {
        Box::pin(async move {
            let query = query.cloned().unwrap_or_default();
            let start = match query.start {
                Some(start) => Some(start),
                None => self.get_tip_id(cx).await?,
            };
            let Some(start) = start else { return Ok(Vec::new()) };
            self.session
                .scan_branch(
                    &StorageBranchScan {
                        start,
                        stop_at_type: query.stop_at_type,
                        stop_at_id: query.stop_at_id,
                        entry_type: query.entry_type,
                        custom_type: query.custom_type,
                        order: Some(query.order.unwrap_or(ScanOrder::Desc)),
                        limit: query.limit,
                        cursor: query.cursor,
                    },
                    cx,
                )
                .await
        })
    }
    fn find_entry<'a>(&'a self, query: Option<&'a BranchScan>, cx: &'a Context) -> BoxFuture<'a, Result<Option<Entry>, SessionError>> {
        Box::pin(async move {
            let mut query = query.cloned().unwrap_or_default();
            query.limit = Some(query.limit.map_or(1, |limit| limit.min(1)));
            Ok(self.find_entries(Some(&query), cx).await?.into_iter().next())
        })
    }
    fn append_message<'a>(&'a self, message: AgentMessage, cx: &'a Context) -> BoxFuture<'a, Result<EntryId, SessionError>> {
        Box::pin(async move { self.session.append_to_branch(&self.name, NewEntryBody::Message { message, terminate: false }, cx).await })
    }
    fn append_custom_entry<'a>(&'a self, custom_type: &'a str, data: Option<serde_json::Value>, cx: &'a Context) -> BoxFuture<'a, Result<EntryId, SessionError>> {
        Box::pin(async move { self.session.append_to_branch(&self.name, NewEntryBody::Custom { custom_type: custom_type.to_owned(), data }, cx).await })
    }
}
