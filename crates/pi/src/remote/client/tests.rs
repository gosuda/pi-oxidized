//! Behavioral coverage for the native v8 client.
//!
//! Cross-implementation Unix/TypeScript smoke recipe (kept as documentation
//! because it needs the upstream package runner): start a Rust endpoint backed
//! by `EndpointSpec::Unix` and a Unix listener, run the upstream
//! `.references/pi/packages/client` client against that socket, complete the
//! v8 hello, issue a generic service request, subscribe through
//! `$chord.service`, and repeat in the opposite direction with the upstream
//! server. The in-memory cases below exercise the same framed byte seam.

use std::sync::{Arc, Mutex as StdMutex};

use pi_agent::context::Context;
use pi_agent::service::binding::{BindingOptions, RemoteServiceBinding};
use pi_agent::service::delta::DeltaOp;
use pi_agent::service::state_codec::ServiceStateEncoder;
use pi_agent::service::value::{JsInteger, JsString, JsonValue, parse_json};
use pi_agent::service::wire::{ServiceMemberSnapshot, ServiceSubscriptionSnapshot};
use tokio::sync::mpsc;

use super::*;
use crate::remote::codec::{ClientMessageDecoder, encode_server_message};
use crate::remote::schemas::{
    ClientMessage, PROTOCOL_VERSION, ProtocolError, RpcTarget, ServerId, ServerMessage,
    ServerTarget, SessionTarget,
};
use crate::remote::transport::{
    ByteTransport, ByteTransportHandlers, EndpointSpec, InMemoryListener, InMemoryTransport,
    TransportError, build_transport,
};

const SERVER_ID: &str = "123e4567-e89b-42d3-a456-426614174000";
const OTHER_SERVER_ID: &str = "223e4567-e89b-42d3-a456-426614174000";

fn value(source: &str) -> JsonValue {
    parse_json(source).expect("valid canonical JSON fixture")
}

fn server_id(value: &str) -> ServerId {
    ServerId::new(value).expect("test server id")
}

fn server_target() -> RpcTarget {
    RpcTarget::Server(ServerTarget {
        server_id: server_id(SERVER_ID),
    })
}

fn service_call(service_id: &str, member: &str) -> ServiceCall {
    ServiceCall {
        service_id: JsString::from_utf8(service_id),
        instance: None,
        member: JsString::from_utf8(member),
        args: Vec::new(),
    }
}

fn make_client(endpoint: crate::remote::transport::InMemoryEndpoint, expected_server: &str) -> Client {
    let factory = build_transport(&EndpointSpec::InMemory { endpoint }).expect("in-memory factory");
    Client::new(ClientOptions {
        transport_factory: factory,
        server_id: expected_server.to_owned(),
        max_frame_length: None,
        on_listener_error: None,
    })
    .expect("client options")
}

struct ServerHandlers {
    decoder: StdMutex<ClientMessageDecoder>,
    sender: mpsc::UnboundedSender<ClientMessage>,
}

impl ByteTransportHandlers for ServerHandlers {
    fn on_data(&self, chunk: Vec<u8>) {
        let messages = super::lock(&self.decoder).push(&chunk).unwrap_or_default();
        for message in messages {
            let _ = self.sender.send(message);
        }
    }

    fn on_close(&self) {}

    fn on_error(&self, _error: TransportError) {}
}

struct ScriptedServer {
    transport: InMemoryTransport,
    messages: mpsc::UnboundedReceiver<ClientMessage>,
}

impl ScriptedServer {
    async fn next(&mut self) -> ClientMessage {
        self.messages.recv().await.expect("client transport remains open")
    }

    async fn send(&self, message: ServerMessage) {
        let frame = encode_server_message(&message, None).expect("server message encodes");
        self.transport.send(frame).await.expect("server transport send");
    }
}

async fn accept_hello(listener: &InMemoryListener, expected_server: &str) -> ScriptedServer {
    let (sender, messages) = mpsc::unbounded_channel();
    let decoder = ClientMessageDecoder::new(None).expect("client decoder");
    let transport = listener
        .accept(Arc::new(ServerHandlers {
            decoder: StdMutex::new(decoder),
            sender,
        }))
        .await
        .expect("listener accepts connection");
    let mut server = ScriptedServer { transport, messages };
    assert!(matches!(server.next().await, ClientMessage::Hello { version: PROTOCOL_VERSION }));
    server
        .send(ServerMessage::Hello {
            version: PROTOCOL_VERSION,
            server_id: server_id(expected_server),
        })
        .await;
    server
}

async fn connected_pair() -> (Client, ScriptedServer) {
    let (listener, endpoint) = InMemoryListener::new();
    let client = make_client(endpoint, SERVER_ID);
    let (connected, server) = tokio::join!(client.connect(), accept_hello(&listener, SERVER_ID));
    connected.expect("hello succeeds");
    (client, server)
}

#[tokio::test]
async fn hello_success_transitions_and_records_hello() {
    let (listener, endpoint) = InMemoryListener::new();
    let client = make_client(endpoint, SERVER_ID);
    let (states, mut state_rx) = mpsc::unbounded_channel();
    let _subscription = client
        .on_connection_state_change(Arc::new(move |change| {
            let _ = states.send(change.state);
        }))
        .expect("listener registration");
    let (connected, server) = tokio::join!(client.connect(), accept_hello(&listener, SERVER_ID));
    let hello = connected.expect("hello succeeds");
    assert_eq!(hello.server_id.as_str(), SERVER_ID);
    assert_eq!(client.server_id(), SERVER_ID);
    assert_eq!(client.hello().expect("hello recorded").server_id.as_str(), SERVER_ID);
    assert!(client.connected());
    assert_eq!(client.connection_state(), ConnectionState::Connected);
    assert_eq!(state_rx.recv().await, Some(ConnectionState::Connecting));
    assert_eq!(state_rx.recv().await, Some(ConnectionState::Connected));
    server.transport.close();
}

#[tokio::test]
async fn hello_wrong_server_id_fails_the_connection() {
    let (listener, endpoint) = InMemoryListener::new();
    let client = make_client(endpoint, SERVER_ID);
    let connect = client.connect();
    let accept = async {
        let (sender, mut messages) = mpsc::unbounded_channel();
        let decoder = ClientMessageDecoder::new(None).expect("client decoder");
        let transport = listener
            .accept(Arc::new(ServerHandlers {
                decoder: StdMutex::new(decoder),
                sender,
            }))
            .await
            .expect("accept");
        assert!(matches!(messages.recv().await, Some(ClientMessage::Hello { .. })));
        let frame = encode_server_message(
            &ServerMessage::Hello {
                version: PROTOCOL_VERSION,
                server_id: server_id(OTHER_SERVER_ID),
            },
            None,
        )
        .expect("hello encodes");
        transport.send(frame).await.expect("send wrong hello");
    };
    let (result, ()) = tokio::join!(connect, accept);
    assert!(matches!(result, Err(ClientError::Protocol(_))));
    assert_eq!(client.connection_state(), ConnectionState::Disconnected);
    assert!(client.hello().is_none());
}

#[tokio::test]
async fn hello_error_becomes_server_error() {
    let (listener, endpoint) = InMemoryListener::new();
    let client = make_client(endpoint, SERVER_ID);
    let connect = client.connect();
    let accept = async {
        let (sender, mut messages) = mpsc::unbounded_channel();
        let decoder = ClientMessageDecoder::new(None).expect("client decoder");
        let transport = listener
            .accept(Arc::new(ServerHandlers {
                decoder: StdMutex::new(decoder),
                sender,
            }))
            .await
            .expect("accept");
        assert!(matches!(messages.recv().await, Some(ClientMessage::Hello { .. })));
        let frame = encode_server_message(
            &ServerMessage::HelloError {
                error: ProtocolError {
                    code: "unsupported_version".to_owned(),
                    message: "v8 required".to_owned(),
                },
            },
            None,
        )
        .expect("hello error encodes");
        transport.send(frame).await.expect("send hello error");
    };
    let (result, ()) = tokio::join!(connect, accept);
    assert!(matches!(
        result,
        Err(ClientError::Server(ServerError { code, message }))
            if code == "unsupported_version" && message == "v8 required"
    ));
    assert_eq!(client.connection_state(), ConnectionState::Disconnected);
}

#[tokio::test]
async fn request_response_correlation_preserves_optional_results() {
    let (client, mut server) = connected_pair().await;
    let target = server_target();
    let first = tokio::spawn({
        let client = client.clone();
        let target = target.clone();
        async move { client.request(target, service_call("demo", "first"), None).await }
    });
    let request = server.next().await;
    let first_id = match request {
        ClientMessage::Request { id, .. } => id,
        other => panic!("expected request, got {other:?}"),
    };
    server
        .send(ServerMessage::Response {
            id: first_id,
            result: Some(value(r#"{"ok":true}"#)),
        })
        .await;
    assert_eq!(
        first.await.expect("request task"),
        Ok(Some(value(r#"{"ok":true}"#)))
    );

    let second = tokio::spawn({
        let client = client.clone();
        let target = target.clone();
        async move { client.request(target, service_call("demo", "second"), None).await }
    });
    let request = server.next().await;
    let second_id = match request {
        ClientMessage::Request { id, .. } => id,
        other => panic!("expected request, got {other:?}"),
    };
    server
        .send(ServerMessage::Response {
            id: second_id,
            result: None,
        })
        .await;
    assert_eq!(second.await.expect("request task"), Ok(None));

    let third = tokio::spawn({
        let client = client.clone();
        async move { client.request(server_target(), service_call("demo", "third"), None).await }
    });
    let request = server.next().await;
    let third_id = match request {
        ClientMessage::Request { id, .. } => id,
        other => panic!("expected request, got {other:?}"),
    };
    server
        .send(ServerMessage::Response {
            id: third_id,
            result: Some(JsonValue::Null),
        })
        .await;
    assert_eq!(
        third.await.expect("request task"),
        Ok(Some(JsonValue::Null))
    );
}

#[tokio::test]
async fn response_error_becomes_server_error() {
    let (client, mut server) = connected_pair().await;
    let request_task = tokio::spawn({
        let client = client.clone();
        async move { client.request(server_target(), service_call("demo", "fail"), None).await }
    });
    let request = server.next().await;
    let id = match request {
        ClientMessage::Request { id, .. } => id,
        other => panic!("expected request, got {other:?}"),
    };
    server
        .send(ServerMessage::ResponseError {
            id,
            error: ProtocolError {
                code: "service_not_found".to_owned(),
                message: "demo is unavailable".to_owned(),
            },
        })
        .await;
    assert!(matches!(
        request_task.await.expect("request task"),
        Err(ClientError::Server(ServerError { code, message }))
            if code == "service_not_found" && message == "demo is unavailable"
    ));
}

#[tokio::test]
async fn cancellation_sends_fenced_cancel_envelope() {
    let (client, mut server) = connected_pair().await;
    let target = server_target();
    let token = CancelToken::new();
    let request = client.request(target.clone(), service_call("demo", "slow"), Some(&token));
    tokio::pin!(request);
    let first = tokio::select! {
        result = &mut request => panic!("request completed before cancellation: {result:?}"),
        message = server.next() => message,
    };
    let id = match first {
        ClientMessage::Request { id, target: request_target, .. } => {
            assert_eq!(request_target, target);
            id
        }
        other => panic!("expected request, got {other:?}"),
    };
    token.cancel();
    assert!(matches!(request.await, Err(ClientError::Cancelled(_))));
    assert!(matches!(
        server.next().await,
        ClientMessage::Cancel { id: cancel_id, target: cancel_target }
            if cancel_id == id && cancel_target == target
    ));
}

#[tokio::test]
async fn attachment_switches_route_and_rejects_wrong_server() {
    let (client, mut server) = connected_pair().await;
    let (changes, mut change_rx) = mpsc::unbounded_channel();
    let _subscription = client
        .on_attachment_change(Arc::new(move |attachment| {
            let _ = changes.send(attachment.clone());
        }))
        .expect("listener registration");
    let attachment = SessionTarget {
        server_id: server_id(SERVER_ID),
        session_id: "session-1".to_owned(),
        attachment_id: "attachment-1".to_owned(),
    };
    server
        .send(ServerMessage::Attachment {
            attachment: Some(attachment.clone()),
        })
        .await;
    assert_eq!(change_rx.recv().await, Some(Some(attachment.clone())));
    assert_eq!(client.attachment(), Some(attachment));

    let wrong_attachment = SessionTarget {
        server_id: server_id(OTHER_SERVER_ID),
        session_id: "session-1".to_owned(),
        attachment_id: "attachment-2".to_owned(),
    };
    server
        .send(ServerMessage::Attachment {
            attachment: Some(wrong_attachment),
        })
        .await;
    assert_eq!(change_rx.recv().await, Some(None));
    assert!(matches!(client.connection_state(), ConnectionState::Disconnected));
    assert!(client.attachment().is_none());
}

#[tokio::test]
async fn reconnect_opens_a_fresh_transport() {
    let (listener, endpoint) = InMemoryListener::new();
    let client = make_client(endpoint, SERVER_ID);
    let first_connect = client.connect();
    let first_accept = accept_hello(&listener, SERVER_ID);
    let (first_result, first_server) = tokio::join!(first_connect, first_accept);
    first_result.expect("first hello");
    client.disconnect("test reconnect");
    first_server.transport.close();

    let second_connect = client.connect();
    let second_accept = accept_hello(&listener, SERVER_ID);
    let (second_result, _second_server) = tokio::join!(second_connect, second_accept);
    second_result.expect("second hello");
    assert_eq!(client.connection_state(), ConnectionState::Connected);
}

#[tokio::test]
async fn service_subscription_queues_updates_until_start() {
    let (client, mut server) = connected_pair().await;
    let (updates, mut update_rx) = mpsc::unbounded_channel();
    let listener: ServiceUpdateListener = Arc::new(move |update| {
        let _ = updates.send(update.clone());
    });
    let target = server_target();
    let subscribe_task = tokio::spawn({
        let client = client.clone();
        let target = target.clone();
        async move {
            client
                .subscribe_service(
                    target,
                    JsString::from_utf8("demo"),
                    ServiceMode::Singleton,
                    listener,
                    None,
                )
                .await
        }
    });
    let request = server.next().await;
    let (request_id, subscription_id) = match request {
        ClientMessage::Request { id, call, .. } => {
            let call = pi_agent::service::wire::parse_service_call(&call)
                .expect("canonical subscribe call");
            assert_eq!(call.service_id, JsString::from_utf8("$chord.service"));
            assert_eq!(call.member, JsString::from_utf8("subscribe"));
            assert_eq!(
                call.args.as_slice(),
                &[
                    JsonValue::String(JsString::from_utf8("service-1")),
                    JsonValue::String(JsString::from_utf8("demo")),
                    JsonValue::String(JsString::from_utf8("singleton")),
                ]
            );
            let subscription_id = match &call.args[0] {
                JsonValue::String(value) => value.try_to_utf8().expect("subscription id"),
                _ => panic!("subscription id must be a string"),
            };
            (id, subscription_id)
        }
        other => panic!("expected subscribe request, got {other:?}"),
    };
    server
        .send(ServerMessage::ServiceUpdate {
            subscription_id: subscription_id.clone(),
            update: value(r#"{"type":"unavailable"}"#),
        })
        .await;
    server
        .send(ServerMessage::Response {
            id: request_id,
            result: Some(value(
                r#"{"serviceId":"demo","mode":"singleton","instances":[]}"#,
            )),
        })
        .await;
    let subscription = subscribe_task
        .await
        .expect("subscription task")
        .expect("subscription response");
    assert!(update_rx.try_recv().is_err());
    subscription.start();
    assert_eq!(
        update_rx.recv().await,
        Some(ServiceProviderUpdate::Unavailable)
    );
    server
        .send(ServerMessage::ServiceUpdate {
            subscription_id,
            update: value(r#"{"type":"unavailable"}"#),
        })
        .await;
    assert_eq!(
        update_rx.recv().await,
        Some(ServiceProviderUpdate::Unavailable)
    );
}

#[tokio::test]
async fn service_state_updates_decode_in_wire_order_before_activation() {
    let (client, mut server) = connected_pair().await;
    let (updates, mut update_rx) = mpsc::unbounded_channel();
    let listener: ServiceUpdateListener = Arc::new(move |update| {
        let _ = updates.send(update.clone());
    });
    let mut encoder = ServiceStateEncoder::new();
    let snapshot = ServiceSubscriptionSnapshot {
        service_id: JsString::from_utf8("demo"),
        mode: ServiceMode::Singleton,
        instances: vec![ServiceInstanceSnapshot {
            instance: None,
            members: vec![ServiceMemberSnapshot::State {
                name: JsString::from_utf8("state"),
                sequence: JsInteger::zero(),
                ops: vec![DeltaOp::replace(value("0"))],
            }],
        }],
    };
    let wire_snapshot = encoder
        .encode_snapshot(&snapshot)
        .expect("snapshot encodes")
        .into_json();
    let update_one = encoder
        .encode_update(&ServiceProviderUpdate::State {
            instance: None,
            member: JsString::from_utf8("state"),
            sequence: JsInteger::one(),
            ops: vec![DeltaOp::replace(value("1"))],
        })
        .expect("first update encodes")
        .into_json();
    let update_two = encoder
        .encode_update(&ServiceProviderUpdate::State {
            instance: None,
            member: JsString::from_utf8("state"),
            sequence: JsInteger::new(2.0).expect("integer sequence"),
            ops: vec![DeltaOp::replace(value("2"))],
        })
        .expect("second update encodes")
        .into_json();
    let subscribe_task = tokio::spawn({
        let client = client.clone();
        async move {
            client
                .subscribe_service(
                    server_target(),
                    JsString::from_utf8("demo"),
                    ServiceMode::Singleton,
                    listener,
                    None,
                )
                .await
        }
    });
    let request = server.next().await;
    let (request_id, subscription_id) = match request {
        ClientMessage::Request { id, call, .. } => {
            let call = pi_agent::service::wire::parse_service_call(&call)
                .expect("canonical subscribe call");
            let subscription_id = match &call.args[0] {
                JsonValue::String(value) => value.try_to_utf8().expect("subscription id"),
                _ => panic!("subscription id must be a string"),
            };
            (id, subscription_id)
        }
        other => panic!("expected subscribe request, got {other:?}"),
    };
    server
        .send(ServerMessage::ServiceUpdate {
            subscription_id: subscription_id.clone(),
            update: update_one,
        })
        .await;
    server
        .send(ServerMessage::Response {
            id: request_id,
            result: Some(wire_snapshot),
        })
        .await;
    let subscription = subscribe_task
        .await
        .expect("subscription task")
        .expect("subscription response");
    server
        .send(ServerMessage::ServiceUpdate {
            subscription_id,
            update: update_two,
        })
        .await;
    assert!(update_rx.try_recv().is_err());
    subscription.start();
    let first = update_rx.recv().await.expect("first decoded update");
    let second = update_rx.recv().await.expect("second decoded update");
    assert!(matches!(
        first,
        ServiceProviderUpdate::State { sequence, .. } if sequence == JsInteger::one()
    ));
    assert!(matches!(
        second,
        ServiceProviderUpdate::State { sequence, .. } if sequence == JsInteger::new(2.0).expect("integer sequence")
    ));
}

#[tokio::test]
async fn client_transport_adapts_to_real_service_binding() {
    let (client, mut server) = connected_pair().await;
    let client = Arc::new(client);
    let service_id = JsString::from_utf8("demo");
    let mut encoder = ServiceStateEncoder::new();
    let snapshot = ServiceSubscriptionSnapshot {
        service_id: service_id.clone(),
        mode: ServiceMode::Singleton,
        instances: vec![ServiceInstanceSnapshot {
            instance: None,
            members: vec![ServiceMemberSnapshot::State {
                name: JsString::from_utf8("state"),
                sequence: JsInteger::zero(),
                ops: vec![DeltaOp::replace(value("0"))],
            }],
        }],
    };
    let wire_snapshot = encoder
        .encode_snapshot(&snapshot)
        .expect("snapshot encodes")
        .into_json();
    let route = server_target();
    let transport = create_client_service_transport(Arc::clone(&client), move || Some(route.clone()));
    let binding = RemoteServiceBinding::new(BindingOptions::new(vec![service_id.clone()], transport))
        .expect("binding");
    let facade = binding.use_service(&service_id).expect("facade");
    let request = server.next().await;
    let (request_id, subscription_id) = match request {
        ClientMessage::Request { id, call, .. } => {
            let call = pi_agent::service::wire::parse_service_call(&call)
                .expect("canonical subscribe call");
            let subscription_id = match &call.args[0] {
                JsonValue::String(value) => value.try_to_utf8().expect("subscription id"),
                _ => panic!("subscription id must be a string"),
            };
            (id, subscription_id)
        }
        other => panic!("expected subscribe request, got {other:?}"),
    };
    server
        .send(ServerMessage::Response {
            id: request_id,
            result: Some(wire_snapshot),
        })
        .await;
    binding
        .ready(&Context::background())
        .await
        .expect("binding hydrates");
    let state = facade.state("state").expect("state member");
    assert_eq!(state.value().as_deref(), Some(&value("0")));
    let update = encoder
        .encode_update(&ServiceProviderUpdate::State {
            instance: None,
            member: JsString::from_utf8("state"),
            sequence: JsInteger::one(),
            ops: vec![DeltaOp::replace(value("1"))],
        })
        .expect("update encodes")
        .into_json();
    server
        .send(ServerMessage::ServiceUpdate {
            subscription_id,
            update,
        })
        .await;
    for _ in 0..32 {
        if state.value().as_deref() == Some(&value("1")) {
            break;
        }
        tokio::task::yield_now().await;
    }
    assert_eq!(state.value().as_deref(), Some(&value("1")));
}

#[tokio::test]
async fn malformed_wire_update_fails_connection() {
    let (client, mut server) = connected_pair().await;
    let (states, mut state_rx) = mpsc::unbounded_channel();
    let _state_listener = client
        .on_connection_state_change(Arc::new(move |change| {
            let _ = states.send(change.clone());
        }))
        .expect("listener registration");
    let listener: ServiceUpdateListener = Arc::new(|_| {});
    let subscribe_task = tokio::spawn({
        let client = client.clone();
        async move {
            client
                .subscribe_service(
                    server_target(),
                    JsString::from_utf8("demo"),
                    ServiceMode::Singleton,
                    listener,
                    None,
                )
                .await
        }
    });
    let request = server.next().await;
    let (request_id, subscription_id) = match request {
        ClientMessage::Request { id, call, .. } => {
            let call = pi_agent::service::wire::parse_service_call(&call)
                .expect("canonical subscribe call");
            let subscription_id = match &call.args[0] {
                JsonValue::String(value) => value.try_to_utf8().expect("subscription id"),
                _ => panic!("subscription id must be a string"),
            };
            (id, subscription_id)
        }
        other => panic!("expected subscribe request, got {other:?}"),
    };
    server
        .send(ServerMessage::Response {
            id: request_id,
            result: Some(value(
                r#"{"serviceId":"demo","mode":"singleton","instances":[]}"#,
            )),
        })
        .await;
    let _subscription = subscribe_task
        .await
        .expect("subscription task")
        .expect("subscription response");
    server
        .send(ServerMessage::ServiceUpdate {
            subscription_id,
            update: value(
                r#"{"type":"state","member":"state","sequence":1,"ops":[true]}"#,
            ),
        })
        .await;
    let change = tokio::time::timeout(std::time::Duration::from_secs(1), async {
        loop {
            let change = state_rx.recv().await.expect("state listener remains active");
            if change.state == ConnectionState::Disconnected {
                break change;
            }
        }
    })
    .await
    .expect("malformed update disconnects the client");
    assert!(matches!(change.error, Some(ClientError::Protocol(_))));
}
