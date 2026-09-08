//! Per-connection session routing and attachment ownership.

use std::collections::{HashMap, HashSet};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex as StdMutex, MutexGuard, PoisonError, Weak};

use futures::future::{BoxFuture, FutureExt, Shared, join_all};
use tokio::sync::{Notify, oneshot};
use uuid::Uuid;

use pi_agent::context::Context;
use pi_agent::service::error::ServiceError;
use pi_agent::service::value::JsonValue;
use pi_agent::service::wire::{ServiceCall, ServiceProviderUpdate};

use crate::remote::schemas::{RpcTarget, ServerId, SessionTarget};

use super::errors::{HostError, ServerError, duplicate_host_error};
use super::host::{PublishUpdate, RoutedServerPresentation, RoutedSessionAttachment, RoutedSessionHandle, ServerHost};

fn lock<T>(mutex: &StdMutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

/// Identity assigned to one accepted connection.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub(crate) struct ClientKey(pub(crate) u64);

struct ClientTail {
    token: Arc<()>,
    future: Shared<BoxFuture<'static, ()>>,
}

struct Opening {
    done: Notify,
}

impl Opening {
    fn new() -> Self {
        Self { done: Notify::new() }
    }
}

struct HostedSession {
    id: String,
    handle: Arc<dyn RoutedSessionHandle>,
    attachments: StdMutex<HashMap<ClientKey, Arc<ClientAttachment>>>,
}

struct OperationTracker {
    active: AtomicUsize,
    releasing: AtomicBool,
    zero: Notify,
}

impl OperationTracker {
    fn new() -> Self {
        Self {
            active: AtomicUsize::new(0),
            releasing: AtomicBool::new(false),
            zero: Notify::new(),
        }
    }

    fn begin(&self) -> Result<(), HostError> {
        if self.releasing.load(Ordering::Acquire) {
            return Err(ServerError::session_not_attached().into());
        }
        self.active.fetch_add(1, Ordering::AcqRel);
        if self.releasing.load(Ordering::Acquire) {
            self.active.fetch_sub(1, Ordering::AcqRel);
            return Err(ServerError::session_not_attached().into());
        }
        Ok(())
    }

    fn end(&self) {
        if self.active.fetch_sub(1, Ordering::AcqRel) == 1 {
            self.zero.notify_waiters();
        }
    }

    async fn finish(&self) {
        self.releasing.store(true, Ordering::Release);
        loop {
            let notified = self.zero.notified();
            if self.active.load(Ordering::Acquire) == 0 {
                return;
            }
            notified.await;
        }
    }
}

struct ReleaseTask {
    done: Notify,
    result: StdMutex<Option<Result<(), HostError>>>,
}

impl ReleaseTask {
    fn new() -> Self {
        Self {
            done: Notify::new(),
            result: StdMutex::new(None),
        }
    }
}

struct ClientAttachment {
    id: String,
    client: ClientKey,
    session: Arc<HostedSession>,
    lease: Arc<dyn RoutedSessionAttachment>,
    operations: Arc<OperationTracker>,
    releasing: StdMutex<Option<Arc<ReleaseTask>>>,
}

pub(super) struct SessionRouterOptions<H: ServerHost> {
    pub(super) host: Arc<H>,
    pub(super) server_id: ServerId,
    pub(super) is_closing: Arc<dyn Fn() -> bool + Send + Sync>,
    pub(super) publish_attachment: Arc<dyn Fn(ClientKey, Option<SessionTarget>, Context) -> BoxFuture<'static, ()> + Send + Sync>,
    pub(super) report_error: Arc<dyn Fn(&dyn std::error::Error) + Send + Sync>,
}

/// Owns durable-session handles and one live attachment per client.
pub(crate) struct SessionRouter<H: ServerHost> {
    options: SessionRouterOptions<H>,
    hosted_sessions: StdMutex<HashMap<String, Arc<HostedSession>>>,
    opening_sessions: StdMutex<HashMap<String, Arc<Opening>>>,
    attachments_by_client: StdMutex<HashMap<ClientKey, Arc<ClientAttachment>>>,
    disconnected_clients: StdMutex<HashSet<ClientKey>>,
    client_operations: StdMutex<HashMap<ClientKey, ClientTail>>,
    closing: AtomicBool,
}

impl<H: ServerHost> SessionRouter<H> {
    pub(crate) fn new(options: SessionRouterOptions<H>) -> Self {
        Self {
            options,
            hosted_sessions: StdMutex::new(HashMap::new()),
            opening_sessions: StdMutex::new(HashMap::new()),
            attachments_by_client: StdMutex::new(HashMap::new()),
            disconnected_clients: StdMutex::new(HashSet::new()),
            client_operations: StdMutex::new(HashMap::new()),
            closing: AtomicBool::new(false),
        }
    }

    pub(crate) fn execute_service_call(
        self: Arc<Self>,
        call: ServiceCall,
        target: RpcTarget,
        client: ClientKey,
        publish: PublishUpdate,
        context: Context,
    ) -> BoxFuture<'static, Result<Option<JsonValue>, HostError>> {
        self.run_for_client(client, move |router| {
            Box::pin(async move {
                router
                    .start_service_call(client, target, call, publish, context)
                    .await
            })
        })
    }

    pub(crate) fn attach_client(
        self: Arc<Self>,
        client: ClientKey,
        session_id: String,
        context: Context,
    ) -> BoxFuture<'static, Result<(), HostError>> {
        self.run_for_client(client, move |router| {
            Box::pin(async move { router.attach_client_now(client, session_id, context).await })
        })
    }

    pub(crate) fn detach_client(
        self: Arc<Self>,
        client: ClientKey,
        context: Context,
    ) -> BoxFuture<'static, Result<(), HostError>> {
        self.run_for_client(client, move |router| {
            Box::pin(async move {
                if let Some(attachment) = router.attachment_for(client) {
                    router.release_attachment(attachment, context, true).await?;
                }
                Ok(())
            })
        })
    }

    pub(crate) fn remove_session(
        self: Arc<Self>,
        session_id: String,
        context: Context,
    ) -> BoxFuture<'static, Result<(), HostError>> {
        Box::pin(async move { self.remove_session_now(&session_id, context).await })
    }

    pub(crate) fn disconnect(
        self: Arc<Self>,
        client: ClientKey,
        context: Context,
    ) -> BoxFuture<'static, Result<(), HostError>> {
        let router = Arc::clone(&self);
        Box::pin(async move {
            lock(&router.disconnected_clients).insert(client);
            let result = router
                .clone()
                .run_for_client(client, move |router| {
                    Box::pin(async move {
                        if let Some(attachment) = router.attachment_for(client) {
                            router.release_attachment(attachment, context, false).await?;
                        }
                        Ok(())
                    })
                })
                .await;
            lock(&router.disconnected_clients).remove(&client);
            result
        })
    }

    pub(crate) fn close(self: Arc<Self>, context: Context) -> BoxFuture<'static, Result<(), HostError>> {
        Box::pin(async move {
            if self.closing.swap(true, Ordering::AcqRel) {
                return Ok(());
            }
            let tails = lock(&self.client_operations)
                .values()
                .map(|tail| tail.future.clone())
                .collect::<Vec<_>>();
            for tail in tails {
                tail.await;
            }

            let attachments = lock(&self.attachments_by_client)
                .values()
                .cloned()
                .collect::<Vec<_>>();
            let release_results = join_all(
                attachments
                    .into_iter()
                    .map(|attachment| self.clone().release_attachment(attachment, context.clone(), false)),
            )
            .await;
            let mut errors = Vec::new();
            for result in release_results {
                if let Err(error) = result {
                    errors.push(error);
                }
            }

            let hosted = lock(&self.hosted_sessions)
                .values()
                .cloned()
                .collect::<Vec<_>>();
            let close_results = join_all(hosted.iter().map(|session| session.handle.close(context.clone())))
                .await;
            for result in close_results {
                if let Err(error) = result {
                    errors.push(error.into());
                }
            }
            lock(&self.hosted_sessions).clear();
            lock(&self.attachments_by_client).clear();
            lock(&self.client_operations).clear();
            if let Some(error) = errors.into_iter().next() {
                return Err(error);
            }
            Ok(())
        })
    }

    fn run_for_client<T, F>(
        self: Arc<Self>,
        client: ClientKey,
        operation: F,
    ) -> BoxFuture<'static, Result<T, HostError>>
    where
        T: Send + 'static,
        F: FnOnce(Arc<Self>) -> BoxFuture<'static, Result<T, HostError>> + Send + 'static,
    {
        let (sender, receiver) = oneshot::channel();
        let weak_router = Arc::downgrade(&self);
        let router = Arc::clone(&self);
        let (task, token) = {
            let mut operations = lock(&self.client_operations);
            let previous = operations
                .get(&client)
                .map(|tail| tail.future.clone())
                .unwrap_or_else(|| futures::future::ready(()).boxed().shared());
            let token = Arc::new(());
            let token_for_task = Arc::clone(&token);
            let task = async move {
                previous.await;
                let result = operation(router).await;
                let _ = sender.send(result);
                if let Some(router) = weak_router.upgrade() {
                    let mut operations = lock(&router.client_operations);
                    if operations
                        .get(&client)
                        .is_some_and(|tail| Arc::ptr_eq(&tail.token, &token_for_task))
                    {
                        operations.remove(&client);
                    }
                }
            }
            .boxed()
            .shared();
            operations.insert(
                client,
                ClientTail {
                    token: Arc::clone(&token),
                    future: task.clone(),
                },
            );
            (task, token)
        };
        drop(token);
        tokio::spawn(task);
        Box::pin(async move {
            receiver
                .await
                .unwrap_or_else(|_| Err(HostError::Protocol("client operation queue closed".to_owned())))
        })
    }

    async fn attach_client_now(
        self: Arc<Self>,
        client: ClientKey,
        session_id: String,
        context: Context,
    ) -> Result<(), HostError> {
        if self.is_closing() || lock(&self.disconnected_clients).contains(&client) {
            return Err(ServerError::server_draining().into());
        }
        let current = self.attachment_for(client);
        if current
            .as_ref()
            .is_some_and(|attachment| attachment.session.id == session_id)
        {
            return Ok(());
        }
        let hosted = self.clone().acquire(session_id.clone(), context.clone()).await?;
        if self.is_closing() || lock(&self.disconnected_clients).contains(&client) {
            return Err(ServerError::server_draining().into());
        }
        if let Some(current) = current {
            self.clone().release_attachment(current, context.clone(), false).await?;
        }
        let attachment = Arc::new(ClientAttachment {
            id: Uuid::new_v4().to_string(),
            client,
            session: Arc::clone(&hosted),
            lease: hosted.handle.attach_client(context.clone()).await?,
            operations: Arc::new(OperationTracker::new()),
            releasing: StdMutex::new(None),
        });
        lock(&hosted.attachments).insert(client, Arc::clone(&attachment));
        if self.is_closing()
            || lock(&self.disconnected_clients).contains(&client)
            || !lock(&self.hosted_sessions)
                .get(&hosted.id)
                .is_some_and(|current| Arc::ptr_eq(current, &hosted))
        {
            self.clone().release_attachment(attachment, context, false).await?;
            return Err(ServerError::server_draining().into());
        }
        lock(&self.attachments_by_client).insert(client, Arc::clone(&attachment));
        (self.options.publish_attachment)(
            client,
            Some(SessionTarget {
                server_id: self.options.server_id.clone(),
                session_id,
                attachment_id: attachment.id.clone(),
            }),
            context,
        )
        .await;
        Ok(())
    }

    async fn start_service_call(
        &self,
        client: ClientKey,
        target: RpcTarget,
        call: ServiceCall,
        publish: PublishUpdate,
        context: Context,
    ) -> Result<Option<JsonValue>, HostError> {
        let attachment = self.require_attachment(client, &target)?;
        attachment.operations.begin()?;
        let result = attachment
            .lease
            .invoke_service(call, publish, context)
            .await;
        attachment.operations.end();
        result
    }

    fn require_attachment(
        &self,
        client: ClientKey,
        target: &RpcTarget,
    ) -> Result<Arc<ClientAttachment>, HostError> {
        if self.is_closing() || lock(&self.disconnected_clients).contains(&client) {
            return Err(ServerError::server_draining().into());
        }
        let RpcTarget::Session(target) = target else {
            return Err(ServerError::session_not_attached().into());
        };
        let Some(attachment) = self.attachment_for(client) else {
            return Err(ServerError::session_not_attached().into());
        };
        if attachment.session.id != target.session_id || attachment.id != target.attachment_id {
            return Err(ServerError::session_not_attached().into());
        }
        Ok(attachment)
    }

    fn attachment_for(&self, client: ClientKey) -> Option<Arc<ClientAttachment>> {
        lock(&self.attachments_by_client).get(&client).cloned()
    }

    async fn release_attachment(
        self: Arc<Self>,
        attachment: Arc<ClientAttachment>,
        context: Context,
        publish: bool,
    ) -> Result<(), HostError> {
        let (task, owner) = {
            let mut releasing = lock(&attachment.releasing);
            if let Some(task) = releasing.as_ref() {
                (Arc::clone(task), false)
            } else {
                let task = Arc::new(ReleaseTask::new());
                *releasing = Some(Arc::clone(&task));
                (task, true)
            }
        };
        if owner {
            let router = Arc::clone(&self);
            let worker_task = Arc::clone(&task);
            tokio::spawn(async move {
                let result = router
                    .release_attachment_inner(Arc::clone(&attachment), context, publish)
                    .await;
                *lock(&worker_task.result) = Some(result);
                worker_task.done.notify_waiters();
            });
        }
        loop {
            let notified = task.done.notified();
            if let Some(result) = lock(&task.result).as_ref() {
                return match result {
                    Ok(()) => Ok(()),
                    Err(error) => Err(duplicate_host_error(error)),
                };
            }
            notified.await;
        }
    }

    async fn release_attachment_inner(
        &self,
        attachment: Arc<ClientAttachment>,
        context: Context,
        publish: bool,
    ) -> Result<(), HostError> {
        attachment.operations.finish().await;
        let lease_result = attachment.lease.release(context.clone()).await;
        self.clear_attachment(&attachment, context.clone(), publish).await;
        match lease_result {
            Ok(()) => Ok(()),
            Err(error) => Err(error),
        }
    }

    async fn clear_attachment(
        &self,
        attachment: &Arc<ClientAttachment>,
        context: Context,
        publish: bool,
    ) {
        lock(&attachment.session.attachments).remove(&attachment.client);
        let removed = {
            let mut by_client = lock(&self.attachments_by_client);
            by_client
                .get(&attachment.client)
                .is_some_and(|current| Arc::ptr_eq(current, attachment))
                .then(|| by_client.remove(&attachment.client))
                .flatten()
                .is_some()
        };
        if removed && publish {
            (self.options.publish_attachment)(attachment.client, None, context).await;
        }
    }

    async fn acquire(
        self: Arc<Self>,
        session_id: String,
        context: Context,
    ) -> Result<Arc<HostedSession>, HostError> {
        loop {
            if let Some(hosted) = lock(&self.hosted_sessions).get(&session_id).cloned() {
                return Ok(hosted);
            }
            let opening = {
                let mut openings = lock(&self.opening_sessions);
                if let Some(opening) = openings.get(&session_id) {
                    Some(Arc::clone(opening))
                } else {
                    let opening = Arc::new(Opening::new());
                    openings.insert(session_id.clone(), Arc::clone(&opening));
                    None
                }
            };
            if let Some(opening) = opening {
                let notified = opening.done.notified();
                context.race(notified).await.map_err(ServiceError::from)?;
                continue;
            }
            let result = self.clone().open(session_id.clone(), context.clone()).await;
            if let Some(opening) = lock(&self.opening_sessions).remove(&session_id) {
                opening.done.notify_waiters();
            }
            return result;
        }
    }

    async fn open(
        self: Arc<Self>,
        session_id: String,
        context: Context,
    ) -> Result<Arc<HostedSession>, HostError> {
        let metadata = self
            .options
            .host
            .resolve_session(session_id.clone(), context.clone())
            .await?;
        let metadata_id = self.options.host.metadata_id(&metadata).to_owned();
        if metadata_id != session_id {
            return Err(ServerError::session_not_found("Session was not found").into());
        }
        let handle = self.options.host.open_session(metadata, context.clone()).await?;
        if self.is_closing() {
            if let Err(error) = handle.close(context).await {
                (self.options.report_error)(&error);
            }
            return Err(ServerError::server_draining().into());
        }
        let hosted = Arc::new(HostedSession {
            id: metadata_id,
            handle,
            attachments: StdMutex::new(HashMap::new()),
        });
        lock(&self.hosted_sessions).insert(hosted.id.clone(), Arc::clone(&hosted));
        if let Some(terminated) = hosted.handle.terminated() {
            let weak = Arc::downgrade(&self);
            tokio::spawn(async move {
                let error = terminated.await;
                if let Some(router) = weak.upgrade() {
                    router.invalidate(hosted, error).await;
                }
            });
        }
        Ok(hosted)
    }

    async fn invalidate(self: Arc<Self>, hosted: Arc<HostedSession>, error: Option<HostError>) {
        let removed = {
            let mut hosted_sessions = lock(&self.hosted_sessions);
            hosted_sessions
                .get(&hosted.id)
                .is_some_and(|current| Arc::ptr_eq(current, &hosted))
                .then(|| hosted_sessions.remove(&hosted.id))
                .flatten()
                .is_some()
        };
        if !removed {
            return;
        }
        let attachments = lock(&hosted.attachments)
            .values()
            .cloned()
            .collect::<Vec<_>>();
        for attachment in attachments {
            if let Err(release_error) = self
                .clone()
                .release_attachment(attachment, Context::background(), true)
                .await
            {
                (self.options.report_error)(&release_error);
            }
        }
        if let Some(error) = error {
            (self.options.report_error)(&error);
        }
    }

    async fn remove_session_now(
        &self,
        session_id: &str,
        context: Context,
    ) -> Result<(), HostError> {
        if self.is_closing() {
            return Err(ServerError::server_draining().into());
        }
        let Some(hosted) = lock(&self.hosted_sessions).get(session_id).cloned() else {
            return Ok(());
        };
        let attachments = lock(&hosted.attachments)
            .values()
            .cloned()
            .collect::<Vec<_>>();
        let releases = join_all(
            attachments
                .into_iter()
                .map(|attachment| self.clone().release_attachment(attachment, context.clone(), true)),
        )
        .await;
        let mut first_error = None;
        for result in releases {
            if let Err(error) = result {
                first_error.get_or_insert(error);
            }
        }
        if let Err(error) = hosted.handle.close(context).await {
            first_error.get_or_insert(error.into());
        }
        if lock(&self.hosted_sessions)
            .get(session_id)
            .is_some_and(|current| Arc::ptr_eq(current, &hosted))
        {
            lock(&self.hosted_sessions).remove(session_id);
        }
        match first_error {
            Some(error) => Err(error),
            None => Ok(()),
        }
    }

    fn is_closing(&self) -> bool {
        self.closing.load(Ordering::Acquire) || (self.options.is_closing)()
    }
}

impl<H: ServerHost> std::fmt::Debug for SessionRouter<H> {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("SessionRouter")
            .field("hosted_sessions", &lock(&self.hosted_sessions).len())
            .field("closing", &self.closing.load(Ordering::Acquire))
            .finish_non_exhaustive()
    }
}

/// Presentation capability supplied to the server-service host for one client.
pub(crate) struct ServerPresentation<H: ServerHost> {
    router: Arc<SessionRouter<H>>,
    client: ClientKey,
}

impl<H: ServerHost> ServerPresentation<H> {
    pub(crate) fn new(router: Arc<SessionRouter<H>>, client: ClientKey) -> Self {
        Self { router, client }
    }
}

impl<H: ServerHost> RoutedServerPresentation for ServerPresentation<H> {
    fn attach_session(&self, session_id: String, context: Context) -> BoxFuture<'_, Result<(), HostError>> {
        self.router
            .clone()
            .attach_client(self.client, session_id, context)
    }

    fn detach_session(&self, context: Context) -> BoxFuture<'_, Result<(), HostError>> {
        self.router.clone().detach_client(self.client, context)
    }

    fn prepare_session_removal(
        &self,
        session_id: String,
        context: Context,
    ) -> BoxFuture<'_, Result<(), HostError>> {
        self.router.clone().remove_session(session_id, context)
    }
}
