//! Native remote-protocol v8 client.
//!
//! This module owns client state, request correlation, attachment routing, and
//! service-subscription delivery. The byte lifecycle and framing decoder live
//! in [`connection`]; service wire values come from `pi_agent`.

mod connection;
mod errors;
#[cfg(test)]
mod tests;
mod transport_adapter;

pub use errors::{
    CancelledError, ClientDisposedError, ClientError, ClientOptionsError, DisconnectedError,
    ProtocolValidationError, ServerError,
};
pub use pi_agent::service::wire::{
    ServiceCall, ServiceCatalogueEntry, ServiceInstanceAddress, ServiceInstanceSnapshot,
    ServiceMemberSnapshot, ServiceMode, ServiceProviderUpdate, ServiceSubscriptionSnapshot,
};
pub use transport_adapter::create_client_service_transport;

use std::collections::{HashMap, VecDeque};
use std::fmt;
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex as StdMutex, MutexGuard, PoisonError, Weak};

use pi_agent::context::Context;
use pi_agent::service::delta::DeltaOp;
use pi_agent::service::state_codec::ServiceStateDecoder;
use pi_agent::service::value::{JsString, JsonValue};
use pi_agent::service::wire::{
    create_service_catalogue_call, create_service_subscribe_call, create_service_unsubscribe_call,
    parse_service_call, parse_service_catalogue, parse_wire_service_provider_update,
    parse_wire_service_subscription_snapshot,
};
use tokio::sync::{Notify, oneshot};
use tokio_util::sync::CancellationToken;

use crate::remote::framing::DEFAULT_MAX_FRAME_LENGTH;
use crate::remote::schemas::{
    ClientMessage, RpcTarget, ServerHello, ServerMessage, SessionTarget, is_server_id,
};
use crate::remote::transport::ByteTransportFactory;

use self::connection::{Connection, ConnectionOptions, open_transport};

/// Maximum updates buffered for one service subscription while delivery is
/// paused: wire frames before the subscribe snapshot hydrates, and decoded
/// updates until [`ServiceSubscription::start`] drains the backlog. A remote
/// endpoint that exceeds the bound fails the connection instead of growing
/// client memory without limit.
const MAX_QUEUED_SERVICE_UPDATES: usize = 4096;

/// A connection-state callback.
pub type ConnectionStateListener = Arc<dyn Fn(&ConnectionStateChange) + Send + Sync>;
/// An attachment-route callback.
pub type AttachmentChangeListener = Arc<dyn Fn(&Option<SessionTarget>) + Send + Sync>;
/// A service-update callback.
pub type ServiceUpdateListener = Arc<dyn Fn(&ServiceProviderUpdate<DeltaOp>) + Send + Sync>;
/// Receives isolated listener failures.
pub type ListenerErrorHandler = Arc<dyn Fn(ClientError) + Send + Sync>;

/// Lifecycle state of one client connection attempt.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConnectionState {
    /// No live transport exists.
    Disconnected,
    /// A transport is opening and the hello handshake is pending.
    Connecting,
    /// The v8 hello handshake completed.
    Connected,
}

impl fmt::Display for ConnectionState {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::Disconnected => "disconnected",
            Self::Connecting => "connecting",
            Self::Connected => "connected",
        })
    }
}

/// One connection lifecycle transition.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConnectionStateChange {
    /// New lifecycle state.
    pub state: ConnectionState,
    /// Failure that caused a transition to [`ConnectionState::Disconnected`].
    pub error: Option<ClientError>,
}

/// A removable listener registration.
pub struct Subscription {
    remove: Option<Box<dyn FnOnce() + Send + Sync>>,
}

impl Subscription {
    fn new(remove: impl FnOnce() + Send + Sync + 'static) -> Self {
        Self {
            remove: Some(Box::new(remove)),
        }
    }
}

impl fmt::Debug for Subscription {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("Subscription")
            .finish_non_exhaustive()
    }
}

impl Drop for Subscription {
    fn drop(&mut self) {
        if let Some(remove) = self.remove.take() {
            remove();
        }
    }
}

/// Cooperative cancellation shared by one or more client operations.
#[derive(Clone)]
pub struct CancelToken {
    state: Arc<CancelState>,
}

struct CancelState {
    cancelled: AtomicBool,
    notify: Notify,
    source: Option<CancellationToken>,
}

impl fmt::Debug for CancelToken {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("CancelToken")
            .field("cancelled", &self.is_cancelled())
            .finish()
    }
}

impl Default for CancelToken {
    fn default() -> Self {
        Self::new()
    }
}

impl CancelToken {
    /// Creates a non-cancelled token.
    #[must_use]
    pub fn new() -> Self {
        Self {
            state: Arc::new(CancelState {
                cancelled: AtomicBool::new(false),
                notify: Notify::new(),
                source: None,
            }),
        }
    }

    /// Bridges a pi-agent context cancellation scope into this client's
    /// cancellation machinery without spawning a duplicate task registry.
    pub(crate) fn from_context(context: &Context) -> Option<Self> {
        context.token().cloned().map(|source| Self {
            state: Arc::new(CancelState {
                cancelled: AtomicBool::new(false),
                notify: Notify::new(),
                source: Some(source),
            }),
        })
    }

    /// Marks the token cancelled. Repeated calls are harmless.
    pub fn cancel(&self) {
        self.state.cancelled.store(true, Ordering::Release);
        self.state.notify.notify_waiters();
    }

    /// Returns whether cancellation was requested.
    #[must_use]
    pub fn is_cancelled(&self) -> bool {
        self.state.cancelled.load(Ordering::Acquire)
            || self
                .state
                .source
                .as_ref()
                .is_some_and(CancellationToken::is_cancelled)
    }

    async fn cancelled(&self) {
        // `notify_waiters` stores no permit, so the future must be created
        // (arming its registration) before the flag is checked; otherwise a
        // cancel racing the pre-check would be lost while this waiter sleeps.
        let notified = self.state.notify.notified();
        tokio::pin!(notified);
        notified.as_mut().enable();
        if self.is_cancelled() {
            return;
        }
        if let Some(source) = self.state.source.clone() {
            tokio::select! {
                biased;
                () = notified => {}
                () = source.cancelled() => {}
            }
        } else {
            notified.await;
        }
    }
}

/// Options for constructing a [`Client`].
#[derive(Clone)]
pub struct ClientOptions {
    /// Factory that opens one fresh byte transport per connection attempt.
    pub transport_factory: ByteTransportFactory,
    /// Canonical lowercase `UUIDv4` identity expected from the endpoint.
    pub server_id: String,
    /// Maximum accepted frame payload; defaults to 16 MiB when omitted.
    pub max_frame_length: Option<usize>,
    /// Receives isolated listener failures, if configured.
    pub on_listener_error: Option<ListenerErrorHandler>,
}

impl fmt::Debug for ClientOptions {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ClientOptions")
            .field("server_id", &self.server_id)
            .field("max_frame_length", &self.max_frame_length)
            .field("on_listener_error", &self.on_listener_error.is_some())
            .finish_non_exhaustive()
    }
}
struct PendingRequest {
    target: RpcTarget,
    sender: oneshot::Sender<Result<Option<JsonValue>, ClientError>>,
}

#[derive(Default)]
struct ActiveServiceState {
    hydrated: bool,
    ready: bool,
    delivering: bool,
    decoder: ServiceStateDecoder,
    queued_wire: Vec<JsonValue>,
    queued: Vec<ServiceProviderUpdate<DeltaOp>>,
}
struct ActiveServiceListener {
    target: RpcTarget,
    listener: ServiceUpdateListener,
    state: StdMutex<ActiveServiceState>,
}

#[derive(Default)]
struct Inner {
    connection: Option<Arc<Connection>>,
    next_connection_id: u64,
    next_request_id: u64,
    next_service_id: u64,
    next_listener_id: u64,
    publishing: bool,
    pending_state_events: VecDeque<(u64, ConnectionStateChange)>,
    pending: HashMap<String, PendingRequest>,
    service_listeners: HashMap<String, Arc<ActiveServiceListener>>,
    connection_state_listeners: HashMap<u64, ConnectionStateListener>,
    attachment_listeners: HashMap<u64, AttachmentChangeListener>,
    hello: Option<ServerHello>,
    attachment: Option<SessionTarget>,
    attachment_generation: u64,
    disposed: bool,
}

/// Shared client state used by [`Client`] and its connection callback seam.
pub(crate) struct ClientCore {
    server_id: String,
    max_frame_length: usize,
    transport_factory: ByteTransportFactory,
    on_listener_error: Option<ListenerErrorHandler>,
    inner: StdMutex<Inner>,
}

/// A remote v8 client over any [`crate::remote::transport::ByteTransport`].
#[derive(Clone)]
pub struct Client {
    core: Arc<ClientCore>,
}

impl fmt::Debug for Client {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("Client")
            .field("server_id", &self.server_id())
            .field("connection_state", &self.connection_state())
            .field("disposed", &self.disposed())
            .finish()
    }
}

/// A live service subscription whose delivery starts explicitly after the
pub struct ServiceSubscription {
    id: String,
    target: RpcTarget,
    snapshot: ServiceSubscriptionSnapshot<DeltaOp>,
    core: Weak<ClientCore>,
    active: Arc<ActiveServiceListener>,
    disposed: AtomicBool,
}

impl fmt::Debug for ServiceSubscription {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ServiceSubscription")
            .field("id", &self.id)
            .field("target", &self.target)
            .field("disposed", &self.disposed.load(Ordering::SeqCst))
            .finish_non_exhaustive()
    }
}

impl Client {
    /// Validates options and creates a disconnected client.
    ///
    /// # Errors
    ///
    /// Returns [`ClientOptionsError::InvalidServerId`] when the server
    /// identity is not a canonical lowercase `UUIDv4`, or
    /// [`ClientOptionsError::InvalidMaxFrameLength`] when the frame limit is
    /// zero or exceeds the protocol's supported range.
    pub fn new(options: ClientOptions) -> Result<Self, ClientOptionsError> {
        if !is_server_id(&options.server_id) {
            return Err(ClientOptionsError::InvalidServerId);
        }
        let max_frame_length = options.max_frame_length.unwrap_or(DEFAULT_MAX_FRAME_LENGTH);
        if !u64::try_from(max_frame_length)
            .is_ok_and(|value| (1..=u64::from(u32::MAX)).contains(&value))
        {
            return Err(ClientOptionsError::InvalidMaxFrameLength {
                value: max_frame_length,
            });
        }
        Ok(Self {
            core: Arc::new(ClientCore {
                server_id: options.server_id,
                max_frame_length,
                transport_factory: options.transport_factory,
                on_listener_error: options.on_listener_error,
                inner: StdMutex::new(Inner::default()),
            }),
        })
    }

    /// Opens a fresh transport and completes the v8 hello handshake.
    ///
    /// # Errors
    ///
    /// Returns [`ClientError`] when the transport, framing, or server
    /// handshake fails.
    pub async fn connect(&self) -> Result<ServerHello, ClientError> {
        self.core.connect().await
    }

    /// Opens a fresh transport after the client is disconnected.
    ///
    /// # Errors
    ///
    /// Returns [`ClientError`] when connecting the fresh transport or
    /// completing the server handshake fails.
    pub async fn reconnect(&self) -> Result<ServerHello, ClientError> {
        self.connect().await
    }

    /// Fails and closes the current connection, if any.
    pub fn disconnect(&self, reason: impl Into<String>) {
        self.core.disconnect(reason.into());
    }

    /// Disposes the client and clears all state and listeners.
    pub fn dispose(&self) {
        self.core.dispose();
    }

    /// Returns the expected logical server identity.
    #[must_use]
    pub fn server_id(&self) -> &str {
        &self.core.server_id
    }

    /// Returns the current connection lifecycle state.
    #[must_use]
    pub fn connection_state(&self) -> ConnectionState {
        self.core.connection_state()
    }

    /// Returns whether the client has completed the hello handshake.
    #[must_use]
    pub fn connected(&self) -> bool {
        self.core.connected()
    }

    /// Returns the last accepted server hello, if connected.
    #[must_use]
    pub fn hello(&self) -> Option<ServerHello> {
        lock(&self.core.inner).hello.clone()
    }

    /// Returns the current attachment route, if attached.
    #[must_use]
    pub fn attachment(&self) -> Option<SessionTarget> {
        lock(&self.core.inner).attachment.clone()
    }

    /// Returns whether disposal has been requested.
    #[must_use]
    pub fn disposed(&self) -> bool {
        lock(&self.core.inner).disposed
    }

    /// Registers a connection-state listener.
    ///
    /// # Errors
    ///
    /// Returns [`ClientError::Disposed`] when the client has already been
    /// disposed.
    pub fn on_connection_state_change(
        &self,
        listener: ConnectionStateListener,
    ) -> Result<Subscription, ClientError> {
        let id = {
            let mut inner = lock(&self.core.inner);
            if inner.disposed {
                return Err(ClientError::Disposed(ClientDisposedError));
            }
            inner.next_listener_id = inner.next_listener_id.wrapping_add(1);
            let id = inner.next_listener_id;
            inner.connection_state_listeners.insert(id, listener);
            id
        };
        let core = Arc::downgrade(&self.core);
        Ok(Subscription::new(move || {
            if let Some(core) = core.upgrade() {
                lock(&core.inner).connection_state_listeners.remove(&id);
            }
        }))
    }

    /// Registers an attachment-route listener.
    ///
    /// # Errors
    ///
    /// Returns [`ClientError::Disposed`] when the client has already been
    /// disposed.
    pub fn on_attachment_change(
        &self,
        listener: AttachmentChangeListener,
    ) -> Result<Subscription, ClientError> {
        let id = {
            let mut inner = lock(&self.core.inner);
            if inner.disposed {
                return Err(ClientError::Disposed(ClientDisposedError));
            }
            inner.next_listener_id = inner.next_listener_id.wrapping_add(1);
            let id = inner.next_listener_id;
            inner.attachment_listeners.insert(id, listener);
            id
        };
        let core = Arc::downgrade(&self.core);
        Ok(Subscription::new(move || {
            if let Some(core) = core.upgrade() {
                lock(&core.inner).attachment_listeners.remove(&id);
            }
        }))
    }

    /// Sends one generic opaque service call to an explicit route.
    ///
    /// # Errors
    ///
    /// Returns [`ClientError`] when the route, service call, connection, or
    /// response is invalid.
    pub async fn request(
        &self,
        target: RpcTarget,
        call: ServiceCall,
        cancel: Option<&CancelToken>,
    ) -> Result<Option<JsonValue>, ClientError> {
        self.core.request(target, call, cancel).await
    }

    /// Requests and validates the service catalogue.
    ///
    /// # Errors
    ///
    /// Returns [`ClientError`] when the request fails or the response is not
    /// a valid service catalogue.
    pub async fn service_catalogue(
        &self,
        target: RpcTarget,
        cancel: Option<&CancelToken>,
    ) -> Result<Vec<ServiceCatalogueEntry>, ClientError> {
        let result = self
            .core
            .request(target, create_service_catalogue_call(), cancel)
            .await?;
        let value = result.unwrap_or(JsonValue::Null);
        match parse_service_catalogue(&value) {
            Ok(catalogue) => Ok(catalogue),
            Err(error) => {
                let error = ClientError::from(error);
                self.core.fail_active_connection(&error);
                Err(error)
            }
        }
    }

    /// Subscribes to one service and defers update delivery until `start`.
    ///
    /// # Errors
    ///
    /// Returns [`ClientError`] when the subscription request or its initial
    /// snapshot is invalid.
    pub async fn subscribe_service(
        &self,
        target: RpcTarget,
        service_id: JsString,
        mode: ServiceMode,
        listener: ServiceUpdateListener,
        cancel: Option<&CancelToken>,
    ) -> Result<ServiceSubscription, ClientError> {
        self.core
            .subscribe_service(target, service_id, mode, listener, cancel)
            .await
    }
}

impl ClientCore {
    async fn connect(self: &Arc<Self>) -> Result<ServerHello, ClientError> {
        let (connection, handshake, publish) = {
            let mut inner = lock(&self.inner);
            if inner.disposed {
                return Err(ClientError::Disposed(ClientDisposedError));
            }
            if let Some(existing) = inner.connection.as_ref() {
                let state = existing.state();
                if state != ConnectionState::Disconnected {
                    return Err(ClientError::disconnected(format!(
                        "Client is already {state}"
                    )));
                }
            }
            inner.hello = None;
            inner.next_connection_id = inner.next_connection_id.wrapping_add(1);
            let connection_id = inner.next_connection_id;
            let options = ConnectionOptions {
                factory: Arc::clone(&self.transport_factory),
                server_id: self.server_id.clone(),
                max_frame_length: self.max_frame_length,
            };
            let (connection, handshake) =
                Connection::new(connection_id, options, Arc::downgrade(self))?;
            inner.connection = Some(Arc::clone(&connection));
            // The transition is queued under the same lock that installs the
            // connection, so a concurrent failure for this connection can
            // only be queued after `Connecting`, never before it.
            let publish = Self::enqueue_connection_state(
                &mut inner,
                connection_id,
                ConnectionStateChange {
                    state: ConnectionState::Connecting,
                    error: None,
                },
            );
            (connection, handshake, publish)
        };
        if publish {
            self.drain_connection_state();
        }
        match tokio::runtime::Handle::try_current() {
            Ok(handle) => {
                handle.spawn(open_transport(connection));
            }
            Err(_) => {
                connection.fail(&ClientError::disconnected(
                    "connect requires a Tokio runtime",
                ));
            }
        }
        handshake.await.unwrap_or_else(|_| {
            Err(ClientError::disconnected(
                "Connection attempt was abandoned",
            ))
        })
    }

    fn disconnect(&self, reason: String) {
        let connection = lock(&self.inner).connection.clone();
        if let Some(connection) = connection {
            connection.fail(&ClientError::disconnected(reason));
        }
    }

    fn dispose(&self) {
        let (connection, pending) = {
            let mut inner = lock(&self.inner);
            if inner.disposed {
                return;
            }
            inner.disposed = true;
            let connection = inner.connection.clone();
            let pending = inner
                .pending
                .drain()
                .map(|(_, pending)| pending.sender)
                .collect::<Vec<_>>();
            (connection, pending)
        };
        for sender in pending {
            let _ = sender.send(Err(ClientError::Disposed(ClientDisposedError)));
        }
        if let Some(connection) = connection {
            connection.fail(&ClientError::Disposed(ClientDisposedError));
        }
        let mut inner = lock(&self.inner);
        inner.hello = None;
        inner.attachment = None;
        inner.service_listeners.clear();
        inner.connection_state_listeners.clear();
        inner.attachment_listeners.clear();
    }

    fn connection_state(&self) -> ConnectionState {
        lock(&self.inner)
            .connection
            .as_ref()
            .map_or(ConnectionState::Disconnected, |connection| {
                connection.state()
            })
    }

    fn connected(&self) -> bool {
        self.connection_state() == ConnectionState::Connected
    }

    async fn request(
        &self,
        target: RpcTarget,
        call: ServiceCall,
        cancel: Option<&CancelToken>,
    ) -> Result<Option<JsonValue>, ClientError> {
        {
            let inner = lock(&self.inner);
            if inner.disposed {
                return Err(ClientError::Disposed(ClientDisposedError));
            }
            if !inner
                .connection
                .as_ref()
                .is_some_and(|connection| connection.state() == ConnectionState::Connected)
            {
                return Err(ClientError::disconnected("Client is disconnected"));
            }
        }
        if cancel.is_some_and(CancelToken::is_cancelled) {
            return Err(ClientError::Cancelled(CancelledError));
        }
        validate_target(&target)?;
        let call_value = call.clone().into_json();
        let call = parse_service_call(&call_value).map_err(ClientError::from)?;
        let call_value = call.into_json();
        let (connection, id, receiver) = {
            let mut inner = lock(&self.inner);
            if inner.disposed {
                return Err(ClientError::Disposed(ClientDisposedError));
            }
            let connection = inner
                .connection
                .clone()
                .filter(|connection| connection.state() == ConnectionState::Connected)
                .ok_or_else(|| ClientError::disconnected("Client is disconnected"))?;
            inner.next_request_id = inner.next_request_id.wrapping_add(1);
            let id = format!("request-{}", inner.next_request_id);
            let (sender, receiver) = oneshot::channel();
            inner.pending.insert(
                id.clone(),
                PendingRequest {
                    target: target.clone(),
                    sender,
                },
            );
            (connection, id, receiver)
        };
        let request = ClientMessage::Request {
            id: id.clone(),
            target: target.clone(),
            call: call_value,
        };
        if let Err(error) = connection.send(&request) {
            self.take_pending(&id);
            return Err(error);
        }
        let mut receiver = receiver;
        match cancel {
            Some(token) => {
                tokio::select! {
                    result = &mut receiver => result.unwrap_or_else(|_| Err(ClientError::disconnected("Client is disconnected"))),
                    () = token.cancelled() => {
                        if self.take_pending(&id).is_some() {
                            if connection.state() == ConnectionState::Connected {
                                let cancel_message = ClientMessage::Cancel {
                                    id: id.clone(),
                                    target,
                                };
                                let _ = connection.send(&cancel_message);
                            }
                            Err(ClientError::Cancelled(CancelledError))
                        } else {
                            receiver.await.unwrap_or_else(|_| Err(ClientError::disconnected("Client is disconnected")))
                        }
                    }
                }
            }
            None => receiver
                .await
                .unwrap_or_else(|_| Err(ClientError::disconnected("Client is disconnected"))),
        }
    }

    async fn subscribe_service(
        self: &Arc<Self>,
        target: RpcTarget,
        service_id: JsString,
        mode: ServiceMode,
        listener: ServiceUpdateListener,
        cancel: Option<&CancelToken>,
    ) -> Result<ServiceSubscription, ClientError> {
        let (subscription_id, active) = {
            let mut inner = lock(&self.inner);
            if inner.disposed {
                return Err(ClientError::Disposed(ClientDisposedError));
            }
            inner.next_service_id = inner.next_service_id.wrapping_add(1);
            let subscription_id = format!("service-{}", inner.next_service_id);
            let active = Arc::new(ActiveServiceListener {
                target: target.clone(),
                listener,
                state: StdMutex::new(ActiveServiceState::default()),
            });
            inner
                .service_listeners
                .insert(subscription_id.clone(), Arc::clone(&active));
            (subscription_id, active)
        };
        let result = self
            .request(
                target.clone(),
                create_service_subscribe_call(subscription_id.clone(), service_id, mode),
                cancel,
            )
            .await;
        let result = match result {
            Ok(result) => result,
            Err(error) => {
                self.remove_service_listener(&subscription_id, &active);
                return Err(error);
            }
        };
        let value = result.unwrap_or(JsonValue::Null);
        let snapshot = {
            let mut state = lock(&active.state);
            let wire_snapshot =
                parse_wire_service_subscription_snapshot(&value).map_err(ClientError::from);
            let wire_snapshot = match wire_snapshot {
                Ok(snapshot) => snapshot,
                Err(error) => {
                    drop(state);
                    self.remove_service_listener(&subscription_id, &active);
                    self.fail_active_connection(&error);
                    return Err(error);
                }
            };
            let snapshot = match state.decoder.decode_snapshot(&wire_snapshot) {
                Ok(snapshot) => snapshot,
                Err(error) => {
                    let error = ClientError::from(error);
                    drop(state);
                    self.remove_service_listener(&subscription_id, &active);
                    self.fail_active_connection(&error);
                    return Err(error);
                }
            };
            let queued_wire = std::mem::take(&mut state.queued_wire);
            for wire_update in queued_wire {
                let wire_update =
                    parse_wire_service_provider_update(&wire_update).map_err(ClientError::from);
                let wire_update = match wire_update {
                    Ok(update) => update,
                    Err(error) => {
                        drop(state);
                        self.remove_service_listener(&subscription_id, &active);
                        self.fail_active_connection(&error);
                        return Err(error);
                    }
                };
                match state.decoder.decode_update(&wire_update) {
                    Ok(update) => state.queued.push(update),
                    Err(error) => {
                        let error = ClientError::from(error);
                        drop(state);
                        self.remove_service_listener(&subscription_id, &active);
                        self.fail_active_connection(&error);
                        return Err(error);
                    }
                }
            }
            state.hydrated = true;
            snapshot
        };
        let still_active = lock(&self.inner)
            .service_listeners
            .get(&subscription_id)
            .is_some_and(|candidate| Arc::ptr_eq(candidate, &active));
        if !still_active {
            return Err(ClientError::disconnected("Client is disconnected"));
        }
        Ok(ServiceSubscription {
            id: subscription_id,
            target,
            snapshot,
            core: Arc::downgrade(self),
            active,
            disposed: AtomicBool::new(false),
        })
    }

    fn remove_service_listener(&self, id: &str, active: &Arc<ActiveServiceListener>) {
        let mut inner = lock(&self.inner);
        if inner
            .service_listeners
            .get(id)
            .is_some_and(|candidate| Arc::ptr_eq(candidate, active))
        {
            inner.service_listeners.remove(id);
        }
    }

    fn take_pending(&self, id: &str) -> Option<PendingRequest> {
        lock(&self.inner).pending.remove(id)
    }

    fn fail_active_connection(&self, error: &ClientError) {
        let connection = lock(&self.inner).connection.clone();
        if let Some(connection) = connection {
            connection.fail(error);
        }
    }

    fn fail_connection(&self, connection_id: u64, error: &ClientError) {
        let connection = {
            let inner = lock(&self.inner);
            inner
                .connection
                .as_ref()
                .filter(|connection| connection.id == connection_id)
                .cloned()
        };
        if let Some(connection) = connection {
            connection.fail(error);
        }
    }

    fn is_current(&self, connection_id: u64) -> bool {
        lock(&self.inner)
            .connection
            .as_ref()
            .is_some_and(|connection| connection.id == connection_id)
    }

    /// Records the accepted handshake for the identified connection.
    ///
    /// Generation validation and mutation are atomic: the handshake is
    /// accepted only while the identified connection is still the registered
    /// connection and has completed its hello, so a handshake completing
    /// around a disconnect or reconnect can never record a superseded
    /// server identity.
    pub(crate) fn on_handshake(&self, connection_id: u64, hello: ServerHello) {
        let publish = {
            let mut inner = lock(&self.inner);
            let live = inner.connection.as_ref().is_some_and(|connection| {
                connection.id == connection_id && connection.state() == ConnectionState::Connected
            });
            if !live || inner.disposed {
                false
            } else {
                inner.hello = Some(hello);
                Self::enqueue_connection_state(
                    &mut inner,
                    connection_id,
                    ConnectionStateChange {
                        state: ConnectionState::Connected,
                        error: None,
                    },
                )
            }
        };
        if publish {
            self.drain_connection_state();
        }
    }

    pub(crate) fn on_message(&self, connection_id: u64, message: ServerMessage) {
        if !self.is_current(connection_id) {
            return;
        }
        match message {
            ServerMessage::Response { id, result } => {
                if let Some(pending) = self.take_pending(&id) {
                    if self.target_is_current(&pending.target) {
                        let _ = pending.sender.send(Ok(result));
                    } else {
                        let _ = pending.sender.send(Err(ClientError::disconnected(
                            "Request route is no longer current",
                        )));
                    }
                } else {
                    self.fail_connection(
                        connection_id,
                        &ClientError::protocol("Response has no matching request"),
                    );
                }
            }
            ServerMessage::ResponseError { id, error } => {
                if error.code.is_empty() {
                    self.fail_connection(
                        connection_id,
                        &ClientError::protocol("Response error has an empty code"),
                    );
                    return;
                }
                if let Some(pending) = self.take_pending(&id) {
                    if self.target_is_current(&pending.target) {
                        let _ = pending.sender.send(Err(error.into()));
                    } else {
                        let _ = pending.sender.send(Err(ClientError::disconnected(
                            "Request route is no longer current",
                        )));
                    }
                } else {
                    self.fail_connection(
                        connection_id,
                        &ClientError::protocol("Response has no matching request"),
                    );
                }
            }
            ServerMessage::ServiceUpdate {
                subscription_id,
                update,
            } => {
                if subscription_id.is_empty() {
                    self.fail_connection(
                        connection_id,
                        &ClientError::protocol("Service update has an empty subscription id"),
                    );
                    return;
                }
                self.on_service_update(connection_id, &subscription_id, &update);
            }
            ServerMessage::Attachment { attachment } => {
                if let Some(attachment) = attachment.as_ref() {
                    if let Err(error) = validate_session_target(attachment) {
                        self.fail_connection(connection_id, &error);
                        return;
                    }
                    if attachment.server_id.as_str() != self.server_id.as_str() {
                        self.fail_connection(
                            connection_id,
                            &ClientError::protocol("Attachment update belongs to another server"),
                        );
                        return;
                    }
                }
                self.set_attachment(connection_id, attachment.as_ref());
            }
            ServerMessage::Hello { .. } | ServerMessage::HelloError { .. } => {
                self.fail_connection(
                    connection_id,
                    &ClientError::protocol("Unexpected handshake message"),
                );
            }
        }
    }

    pub(crate) fn on_disconnected(&self, connection_id: u64, error: &ClientError) {
        let (pending, attachment_generation, publish) = {
            let mut inner = lock(&self.inner);
            if inner
                .connection
                .as_ref()
                .is_none_or(|connection| connection.id != connection_id)
            {
                return;
            }
            inner.hello = None;
            let attachment_generation = if inner.attachment.take().is_some() {
                inner.attachment_generation = inner.attachment_generation.wrapping_add(1);
                Some(inner.attachment_generation)
            } else {
                None
            };
            let pending = inner
                .pending
                .drain()
                .map(|(_, pending)| pending.sender)
                .collect::<Vec<_>>();
            inner.service_listeners.clear();
            // The transition is queued under the same lock that performs the
            // cleanup, so a reconnect racing in afterwards can only be queued
            // after `Disconnected`. At dispatch time the transition is
            // published only while it still describes the registered
            // connection, so a superseded connection can never publish
            // `Disconnected` after a newer `Connecting`.
            let publish = Self::enqueue_connection_state(
                &mut inner,
                connection_id,
                ConnectionStateChange {
                    state: ConnectionState::Disconnected,
                    error: Some(error.clone()),
                },
            );
            (pending, attachment_generation, publish)
        };
        if attachment_generation
            .is_some_and(|generation| self.attachment_generation_is_current(generation))
        {
            self.fire_attachment(&None);
        }
        for sender in pending {
            let _ = sender.send(Err(error.clone()));
        }
        if publish {
            self.drain_connection_state();
        }
    }

    fn on_service_update(&self, connection_id: u64, subscription_id: &str, update: &JsonValue) {
        let active = lock(&self.inner)
            .service_listeners
            .get(subscription_id)
            .cloned();
        let Some(active) = active else {
            return;
        };
        if !self.target_is_current(&active.target) {
            return;
        }
        let parsed = {
            let mut state = lock(&active.state);
            if !state.hydrated {
                if state.queued_wire.len() >= MAX_QUEUED_SERVICE_UPDATES {
                    drop(state);
                    self.fail_connection(
                        connection_id,
                        &ClientError::protocol(
                            "Service updates queued before subscription start exceeded the limit",
                        ),
                    );
                    return;
                }
                state.queued_wire.push(update.clone());
                return;
            }
            let wire_update =
                match parse_wire_service_provider_update(update).map_err(ClientError::from) {
                    Ok(update) => update,
                    Err(error) => {
                        drop(state);
                        self.fail_connection(connection_id, &error);
                        return;
                    }
                };
            match state.decoder.decode_update(&wire_update) {
                Ok(update) if state.ready && !state.delivering => Some(update),
                Ok(update) => {
                    if state.queued.len() >= MAX_QUEUED_SERVICE_UPDATES {
                        drop(state);
                        self.fail_connection(
                            connection_id,
                            &ClientError::protocol(
                                "Service updates queued before subscription start exceeded the limit",
                            ),
                        );
                        return;
                    }
                    state.queued.push(update);
                    None
                }
                Err(error) => {
                    let error = ClientError::from(error);
                    drop(state);
                    self.fail_connection(connection_id, &error);
                    return;
                }
            }
        };
        if let Some(update) = parsed {
            self.deliver_service_update(&active, &update);
        }
    }

    fn deliver_service_update(
        &self,
        active: &ActiveServiceListener,
        update: &ServiceProviderUpdate<DeltaOp>,
    ) {
        let registered = lock(&self.inner)
            .service_listeners
            .values()
            .any(|candidate| std::ptr::eq(candidate.as_ref(), active));
        if !registered || !self.target_is_current(&active.target) {
            return;
        }
        let listener = Arc::clone(&active.listener);
        if let Err(payload) = catch_unwind(AssertUnwindSafe(|| listener(update))) {
            self.report_listener_error(ClientError::protocol(format!(
                "Service listener panicked: {}",
                panic_message(&payload),
            )));
        }
    }

    /// Installs an attachment route delivered by the identified connection.
    ///
    /// Currency and disposal are re-checked under the same lock that performs
    /// the write, so a frame decoded around a disconnect or reconnect can
    /// never reinstall a superseded route. Listeners observe the exact value
    /// this call installed, unless a concurrent transition already superseded
    /// it; the superseding path fires its own value.
    fn set_attachment(&self, connection_id: u64, attachment: Option<&SessionTarget>) {
        let (generation, value) = {
            let mut inner = lock(&self.inner);
            let live = inner.connection.as_ref().is_some_and(|connection| {
                connection.id == connection_id && connection.state() == ConnectionState::Connected
            });
            if !live || inner.disposed || inner.attachment.as_ref() == attachment {
                return;
            }
            let value = attachment.cloned();
            inner.attachment.clone_from(&value);
            inner.attachment_generation = inner.attachment_generation.wrapping_add(1);
            (inner.attachment_generation, value)
        };
        if !self.attachment_generation_is_current(generation) {
            return;
        }
        self.fire_attachment(&value);
    }

    /// Returns whether `generation` still describes the attachment route and
    /// the client has not been disposed since the transition was recorded.
    fn attachment_generation_is_current(&self, generation: u64) -> bool {
        let inner = lock(&self.inner);
        !inner.disposed && inner.attachment_generation == generation
    }

    fn target_is_current(&self, target: &RpcTarget) -> bool {
        let inner = lock(&self.inner);
        match target {
            RpcTarget::Server(server) => inner
                .hello
                .as_ref()
                .is_some_and(|hello| hello.server_id == server.server_id),
            RpcTarget::Session(session) => inner.attachment.as_ref().is_some_and(|attachment| {
                attachment.server_id == session.server_id
                    && attachment.session_id == session.session_id
                    && attachment.attachment_id == session.attachment_id
            }),
        }
    }

    /// Queues one connection-state transition under `inner`'s lock.
    ///
    /// Returns whether the caller claimed the publisher role and must call
    /// [`ClientCore::drain_connection_state`] after releasing `inner`.
    fn enqueue_connection_state(
        inner: &mut Inner,
        connection_id: u64,
        change: ConnectionStateChange,
    ) -> bool {
        inner.pending_state_events.push_back((connection_id, change));
        if inner.publishing {
            return false;
        }
        inner.publishing = true;
        true
    }

    /// Returns the next queued transition, releasing the publisher role when
    /// the queue is empty.
    fn pop_connection_state(inner: &mut Inner) -> Option<(u64, ConnectionStateChange)> {
        let next = inner.pending_state_events.pop_front();
        if next.is_none() {
            inner.publishing = false;
        }
        next
    }

    /// Publishes queued connection-state transitions to listeners.
    ///
    /// Publication is serialized: transitions are queued under `inner`'s lock
    /// in exactly the order the lifecycle produced them, and only one thread
    /// dispatches at a time. Each transition is published only while it still
    /// describes the registered connection, so a superseded connection can
    /// never observe its `Disconnected` after a newer `Connecting`. Callbacks
    /// run with no lock held; a listener that synchronously triggers another
    /// transition only queues it, and this drain publishes it in turn.
    fn drain_connection_state(&self) {
        loop {
            // The guard is confined to this block: listener callbacks below
            // take `inner` themselves, and the std mutex is not reentrant.
            let next = {
                let mut inner = lock(&self.inner);
                Self::pop_connection_state(&mut inner)
            };
            let Some((connection_id, change)) = next else {
                break;
            };
            if !self.is_current(connection_id) {
                continue;
            }
            let listeners = lock(&self.inner)
                .connection_state_listeners
                .values()
                .cloned()
                .collect::<Vec<_>>();
            for listener in listeners {
                if let Err(payload) = catch_unwind(AssertUnwindSafe(|| listener(&change))) {
                    self.report_listener_error(ClientError::protocol(format!(
                        "Connection-state listener panicked: {}",
                        panic_message(&payload),
                    )));
                }
            }
        }
    }

    #[expect(
        clippy::ref_option,
        reason = "AttachmentChangeListener callback receives &Option<SessionTarget>"
    )]
    fn fire_attachment(&self, attachment: &Option<SessionTarget>) {
        let listeners = lock(&self.inner)
            .attachment_listeners
            .values()
            .cloned()
            .collect::<Vec<_>>();
        for listener in listeners {
            if let Err(payload) = catch_unwind(AssertUnwindSafe(|| listener(attachment))) {
                self.report_listener_error(ClientError::protocol(format!(
                    "Attachment listener panicked: {}",
                    panic_message(&payload),
                )));
            }
        }
    }

    fn report_listener_error(&self, error: ClientError) {
        let Some(handler) = self.on_listener_error.clone() else {
            return;
        };
        let _ = catch_unwind(AssertUnwindSafe(|| handler(error)));
    }
}
fn validate_target(target: &RpcTarget) -> Result<(), ClientError> {
    if let RpcTarget::Session(session) = target {
        validate_session_target(session)?;
    }
    Ok(())
}

fn validate_session_target(session: &SessionTarget) -> Result<(), ClientError> {
    if session.session_id.is_empty() || session.attachment_id.is_empty() {
        return Err(ClientError::protocol(
            "Session target identifiers must be non-empty",
        ));
    }
    Ok(())
}

impl ServiceSubscription {
    /// Returns the generated subscription identifier.
    #[must_use]
    pub fn id(&self) -> &str {
        &self.id
    }

    /// Returns the route used by this subscription.
    #[must_use]
    pub fn target(&self) -> &RpcTarget {
        &self.target
    }

    /// Returns the validated initial provider snapshot.
    #[must_use]
    pub fn snapshot(&self) -> &ServiceSubscriptionSnapshot<DeltaOp> {
        &self.snapshot
    }

    /// Starts ordered delivery of updates queued before the snapshot was
    /// ready, then drains everything that arrives while the backlog drains.
    ///
    /// Live updates are captured behind the in-flight drain until it
    /// quiesces, so listeners observe wire order instead of task scheduling
    /// order. Callbacks still run with no lock held, keeping listener
    /// re-entry (including `start` itself, which is idempotent) deadlock
    /// free.
    pub fn start(&self) {
        if self.disposed.load(Ordering::SeqCst) {
            return;
        }
        let Some(core) = self.core.upgrade() else {
            return;
        };
        let mut batch = {
            let mut state = lock(&self.active.state);
            if state.ready {
                return;
            }
            state.ready = true;
            state.delivering = true;
            std::mem::take(&mut state.queued)
        };
        loop {
            for update in batch {
                core.deliver_service_update(&self.active, &update);
            }
            let next = {
                let mut state = lock(&self.active.state);
                if state.queued.is_empty() {
                    state.delivering = false;
                    None
                } else {
                    Some(std::mem::take(&mut state.queued))
                }
            };
            let Some(queued) = next else {
                break;
            };
            batch = queued;
        }
    }

    /// Removes this listener and, when still routable, sends unsubscribe.
    ///
    /// # Errors
    ///
    /// Returns [`ClientError`] when the unsubscribe request fails. Disposal
    /// remains idempotent and succeeds when the client is already gone.
    pub async fn dispose(&self) -> Result<(), ClientError> {
        if self.disposed.swap(true, Ordering::SeqCst) {
            return Ok(());
        }
        let Some(core) = self.core.upgrade() else {
            return Ok(());
        };
        core.remove_service_listener(&self.id, &self.active);
        let result = if core.connected() && core.target_is_current(&self.target) {
            core.request(
                self.target.clone(),
                create_service_unsubscribe_call(self.id.clone()),
                None,
            )
            .await
            .map(|_| ())
        } else {
            Ok(())
        };
        let mut state = lock(&self.active.state);
        state.queued_wire.clear();
        state.queued.clear();
        result
    }
}
fn panic_message(payload: &(dyn std::any::Any + Send)) -> String {
    if let Some(message) = payload.downcast_ref::<&str>() {
        return (*message).to_string();
    }
    if let Some(message) = payload.downcast_ref::<String>() {
        return message.clone();
    }
    "unknown panic payload".to_string()
}

/// Recovers a poisoned mutex without turning listener activity into a panic.
pub(crate) fn lock<T>(mutex: &StdMutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}
