//! Strict native remote protocol v8 schemas.
//!
//! The remote protocol carries routing and opaque service values only. Session,
//! model, transcript, and service-domain records belong to the service payload,
//! not to this envelope module.

use std::fmt;
use std::str::FromStr;

use serde::de::DeserializeOwned;
use serde::ser::{SerializeStruct, Serializer};
use serde::{Deserialize, Deserializer, Serialize};

use pi_agent::service::value::JsonValue;

use crate::remote::serde_cbor::{opaque_json, CborValue, CborValueDeserializer, OpaqueJson};

/// Protocol version implemented by the native remote wire.
pub const PROTOCOL_VERSION: u64 = 8;

/// Protocol error codes are open, non-empty strings.
///
/// This alias intentionally has no closed list of variants. The codec checks
/// the non-empty invariant at the protocol boundary.
pub type ProtocolErrorCode = String;

/// A protocol-level error returned by the server.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProtocolError {
    /// Open machine-readable error code.
    pub code: ProtocolErrorCode,
    /// Human-readable error description.
    pub message: String,
}

/// Error returned when a server identifier is not a canonical lowercase UUIDv4.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ServerIdError;

impl fmt::Display for ServerIdError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("server id must be a lowercase UUIDv4")
    }
}

impl std::error::Error for ServerIdError {}

/// The server identity used to fence every remote route.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct ServerId(String);

impl ServerId {
    /// Creates a server identifier after validating its exact wire spelling.
    pub fn new(value: impl Into<String>) -> Result<Self, ServerIdError> {
        let value = value.into();
        if is_server_id(&value) {
            Ok(Self(value))
        } else {
            Err(ServerIdError)
        }
    }

    /// Borrows the canonical UUIDv4 spelling.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl TryFrom<String> for ServerId {
    type Error = ServerIdError;

    fn try_from(value: String) -> Result<Self, Self::Error> {
        Self::new(value)
    }
}

impl TryFrom<&str> for ServerId {
    type Error = ServerIdError;

    fn try_from(value: &str) -> Result<Self, Self::Error> {
        Self::new(value)
    }
}

impl FromStr for ServerId {
    type Err = ServerIdError;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        Self::try_from(value)
    }
}

impl From<ServerId> for String {
    fn from(value: ServerId) -> Self {
        value.0
    }
}

impl AsRef<str> for ServerId {
    fn as_ref(&self) -> &str {
        self.as_str()
    }
}

impl fmt::Display for ServerId {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.as_str())
    }
}

impl Serialize for ServerId {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        serializer.serialize_str(self.as_str())
    }
}

impl<'de> Deserialize<'de> for ServerId {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let value = String::deserialize(deserializer)?;
        Self::new(value).map_err(serde::de::Error::custom)
    }
}

/// Returns whether `value` is an exact lowercase UUIDv4 server identifier.
#[must_use]
pub fn is_server_id(value: &str) -> bool {
    let bytes = value.as_bytes();
    if bytes.len() != 36 {
        return false;
    }
    for (index, byte) in bytes.iter().copied().enumerate() {
        if matches!(index, 8 | 13 | 18 | 23) {
            if byte != b'-' {
                return false;
            }
            continue;
        }
        if index == 14 {
            if byte != b'4' {
                return false;
            }
            continue;
        }
        if index == 19 {
            if !matches!(byte, b'8' | b'9' | b'a' | b'b') {
                return false;
            }
            continue;
        }
        if !matches!(byte, b'0'..=b'9' | b'a'..=b'f') {
            return false;
        }
    }
    true
}

/// A server-wide route fenced to one logical server.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ServerTarget {
    /// Fenced server identity.
    #[serde(rename = "serverId")]
    pub server_id: ServerId,
}

/// A session route fenced to a server, durable session, and live attachment.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SessionTarget {
    /// Fenced server identity.
    #[serde(rename = "serverId")]
    pub server_id: ServerId,
    /// Durable session identifier.
    #[serde(rename = "sessionId")]
    pub session_id: String,
    /// Server-issued attachment identity for this live route.
    #[serde(rename = "attachmentId")]
    pub attachment_id: String,
}

/// A server-wide or session-scoped route.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum RpcTarget {
    /// Server-wide route.
    Server(ServerTarget),
    /// Session route.
    Session(SessionTarget),
}

/// The first client envelope used to negotiate a protocol version.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ClientHello {
    /// Non-negative integer offered by the client.
    pub version: u64,
}

/// A client request envelope carrying an opaque service call.
#[derive(Debug, Clone, PartialEq)]
pub struct RequestEnvelope {
    /// Correlation identifier.
    pub id: String,
    /// Fenced route for the call.
    pub target: RpcTarget,
    /// Strict JSON service call owned by the service layer.
    pub call: JsonValue,
}

/// A client cancellation envelope.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CancelEnvelope {
    /// Correlation identifier of the request to cancel.
    pub id: String,
    /// Fenced route for the cancellation.
    pub target: RpcTarget,
}

/// A successful server handshake envelope.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ServerHello {
    /// Server protocol version. A valid server hello always carries 8.
    pub version: u64,
    /// Server identity used to fence routes.
    pub server_id: ServerId,
}

/// A failed server handshake envelope.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ServerHelloError {
    /// Protocol error explaining why negotiation failed.
    pub error: ProtocolError,
}

/// A response to a request, split so success and failure cannot be mixed.
#[derive(Debug, Clone, PartialEq)]
pub enum ResponseEnvelope {
    /// Successful response; `None` means the result key is absent.
    Success {
        /// Correlation identifier.
        id: String,
        /// Optional result. `Some(JsonValue::Null)` is an explicit JSON null.
        result: Option<JsonValue>,
    },
    /// Failed response.
    Error {
        /// Correlation identifier.
        id: String,
        /// Protocol error returned by the service endpoint.
        error: ProtocolError,
    },
}

/// A service subscription update carrying an opaque strict JSON value.
#[derive(Debug, Clone, PartialEq)]
pub struct ServiceEventEnvelope {
    /// Subscription receiving this update.
    pub subscription_id: String,
    /// Service-defined update payload.
    pub update: JsonValue,
}

/// An out-of-band update to the selected session route.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AttachmentEnvelope {
    /// Current attachment route, or `None` when detached.
    pub attachment: Option<SessionTarget>,
}

/// A message sent from a client to a remote server.
#[derive(Debug, Clone, PartialEq)]
pub enum ClientMessage {
    /// Initial version negotiation.
    Hello {
        /// Non-negative integer offered by the client.
        version: u64,
    },
    /// Opaque service request.
    Request {
        /// Correlation identifier.
        id: String,
        /// Fenced route for the call.
        target: RpcTarget,
        /// Strict JSON service call.
        call: JsonValue,
    },
    /// Cancellation of a request at a fenced route.
    Cancel {
        /// Correlation identifier.
        id: String,
        /// Fenced route for the cancellation.
        target: RpcTarget,
    },
}

/// A message sent from a remote server to a client.
#[derive(Debug, Clone, PartialEq)]
pub enum ServerMessage {
    /// Successful version negotiation.
    Hello {
        /// Server protocol version, exactly 8 for a valid message.
        version: u64,
        /// Server identity used to fence routes.
        server_id: ServerId,
    },
    /// Failed version negotiation.
    HelloError {
        /// Protocol error explaining the rejection.
        error: ProtocolError,
    },
    /// Successful request response.
    Response {
        /// Correlation identifier.
        id: String,
        /// Optional result; `Some(JsonValue::Null)` is explicit JSON null.
        result: Option<JsonValue>,
    },
    /// Failed request response.
    ResponseError {
        /// Correlation identifier.
        id: String,
        /// Protocol error returned by the endpoint.
        error: ProtocolError,
    },
    /// Opaque service subscription update.
    ServiceUpdate {
        /// Subscription receiving this update.
        subscription_id: String,
        /// Service-defined update payload.
        update: JsonValue,
    },
    /// Attachment route switch, including an explicit detached `None` state.
    Attachment {
        /// Current session route, or `None` when detached.
        attachment: Option<SessionTarget>,
    },
}

impl From<ClientHello> for ClientMessage {
    fn from(value: ClientHello) -> Self {
        Self::Hello {
            version: value.version,
        }
    }
}

impl From<RequestEnvelope> for ClientMessage {
    fn from(value: RequestEnvelope) -> Self {
        Self::Request {
            id: value.id,
            target: value.target,
            call: value.call,
        }
    }
}

impl From<CancelEnvelope> for ClientMessage {
    fn from(value: CancelEnvelope) -> Self {
        Self::Cancel {
            id: value.id,
            target: value.target,
        }
    }
}

impl From<ServerHello> for ServerMessage {
    fn from(value: ServerHello) -> Self {
        Self::Hello {
            version: value.version,
            server_id: value.server_id,
        }
    }
}

impl From<ServerHelloError> for ServerMessage {
    fn from(value: ServerHelloError) -> Self {
        Self::HelloError { error: value.error }
    }
}

impl From<ResponseEnvelope> for ServerMessage {
    fn from(value: ResponseEnvelope) -> Self {
        match value {
            ResponseEnvelope::Success { id, result } => Self::Response { id, result },
            ResponseEnvelope::Error { id, error } => Self::ResponseError { id, error },
        }
    }
}

impl From<ServiceEventEnvelope> for ServerMessage {
    fn from(value: ServiceEventEnvelope) -> Self {
        Self::ServiceUpdate {
            subscription_id: value.subscription_id,
            update: value.update,
        }
    }
}

impl From<AttachmentEnvelope> for ServerMessage {
    fn from(value: AttachmentEnvelope) -> Self {
        Self::Attachment {
            attachment: value.attachment,
        }
    }
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct ClientHelloWire {
    #[serde(rename = "type")]
    type_field: String,
    version: u64,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RequestWire {
    #[serde(rename = "type")]
    type_field: String,
    id: String,
    target: RpcTarget,
    #[serde(deserialize_with = "opaque_json::deserialize")]
    call: JsonValue,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct CancelWire {
    #[serde(rename = "type")]
    type_field: String,
    id: String,
    target: RpcTarget,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct ServerHelloWire {
    #[serde(rename = "type")]
    type_field: String,
    version: u64,
    #[serde(rename = "serverId")]
    server_id: ServerId,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct ServerHelloErrorWire {
    #[serde(rename = "type")]
    type_field: String,
    error: ProtocolError,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct ResponseSuccessWire {
    #[serde(rename = "type")]
    type_field: String,
    id: String,
    ok: bool,
    #[serde(default, deserialize_with = "deserialize_optional_json")]
    result: Option<JsonValue>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct ResponseErrorWire {
    #[serde(rename = "type")]
    type_field: String,
    id: String,
    ok: bool,
    error: ProtocolError,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct ServiceEventWire {
    #[serde(rename = "type")]
    type_field: String,
    #[serde(rename = "subscriptionId")]
    subscription_id: String,
    #[serde(deserialize_with = "opaque_json::deserialize")]
    update: JsonValue,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct AttachmentWire {
    #[serde(rename = "type")]
    type_field: String,
    attachment: Option<SessionTarget>,
}

fn deserialize_optional_json<'de, D>(deserializer: D) -> Result<Option<JsonValue>, D::Error>
where
    D: Deserializer<'de>,
{
    opaque_json::deserialize(deserializer).map(Some)
}

fn object_type(value: &CborValue) -> Result<&str, String> {
    let CborValue::Map(entries) = value else {
        return Err("protocol message must be an object".to_owned());
    };
    entries
        .iter()
        .find(|(key, _)| key == "type")
        .and_then(|(_, value)| match value {
            CborValue::Text(value) => Some(value.as_str()),
            _ => None,
        })
        .ok_or_else(|| "protocol message type must be a string".to_owned())
}

fn decode_wire<T: DeserializeOwned>(value: CborValue) -> Result<T, String> {
    T::deserialize(CborValueDeserializer { value }).map_err(|error| error.to_string())
}

fn require_type(actual: &str, expected: &str) -> Result<(), String> {
    if actual == expected {
        Ok(())
    } else {
        Err(format!("expected message type `{expected}`"))
    }
}

fn parse_client_value(value: CborValue) -> Result<ClientMessage, String> {
    let type_name = object_type(&value)?;
    match type_name {
        "hello" => {
            let wire: ClientHelloWire = decode_wire(value)?;
            require_type(&wire.type_field, "hello")?;
            Ok(ClientMessage::Hello {
                version: wire.version,
            })
        }
        "request" => {
            let wire: RequestWire = decode_wire(value)?;
            require_type(&wire.type_field, "request")?;
            Ok(ClientMessage::Request {
                id: wire.id,
                target: wire.target,
                call: wire.call,
            })
        }
        "cancel" => {
            let wire: CancelWire = decode_wire(value)?;
            require_type(&wire.type_field, "cancel")?;
            Ok(ClientMessage::Cancel {
                id: wire.id,
                target: wire.target,
            })
        }
        _ => Err(format!("unknown variant `{type_name}`")),
    }
}

fn parse_response_value(value: CborValue) -> Result<ResponseEnvelope, String> {
    let type_name = object_type(&value)?;
    require_type(type_name, "response")?;
    let ok = match &value {
        CborValue::Map(entries) => entries
            .iter()
            .find(|(key, _)| key == "ok")
            .and_then(|(_, value)| match value {
                CborValue::Bool(value) => Some(*value),
                _ => None,
            }),
        _ => None,
    }
    .ok_or_else(|| "response field `ok` must be a boolean".to_owned())?;
    if ok {
        let wire: ResponseSuccessWire = decode_wire(value)?;
        require_type(&wire.type_field, "response")?;
        if !wire.ok {
            return Err("response success must carry ok=true".to_owned());
        }
        Ok(ResponseEnvelope::Success {
            id: wire.id,
            result: wire.result,
        })
    } else {
        let wire: ResponseErrorWire = decode_wire(value)?;
        require_type(&wire.type_field, "response")?;
        if wire.ok {
            return Err("response error must carry ok=false".to_owned());
        }
        Ok(ResponseEnvelope::Error {
            id: wire.id,
            error: wire.error,
        })
    }
}

fn parse_server_value(value: CborValue) -> Result<ServerMessage, String> {
    let type_name = object_type(&value)?;
    match type_name {
        "hello" => {
            let wire: ServerHelloWire = decode_wire(value)?;
            require_type(&wire.type_field, "hello")?;
            Ok(ServerMessage::Hello {
                version: wire.version,
                server_id: wire.server_id,
            })
        }
        "hello_error" => {
            let wire: ServerHelloErrorWire = decode_wire(value)?;
            require_type(&wire.type_field, "hello_error")?;
            Ok(ServerMessage::HelloError { error: wire.error })
        }
        "response" => match parse_response_value(value)? {
            ResponseEnvelope::Success { id, result } => Ok(ServerMessage::Response { id, result }),
            ResponseEnvelope::Error { id, error } => Ok(ServerMessage::ResponseError { id, error }),
        },
        "service_update" => {
            let wire: ServiceEventWire = decode_wire(value)?;
            require_type(&wire.type_field, "service_update")?;
            Ok(ServerMessage::ServiceUpdate {
                subscription_id: wire.subscription_id,
                update: wire.update,
            })
        }
        "attachment" => {
            let wire: AttachmentWire = decode_wire(value)?;
            require_type(&wire.type_field, "attachment")?;
            Ok(ServerMessage::Attachment {
                attachment: wire.attachment,
            })
        }
        _ => Err(format!("unknown variant `{type_name}`")),
    }
}

impl Serialize for ClientHello {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        let mut state = serializer.serialize_struct("ClientHello", 2)?;
        state.serialize_field("type", "hello")?;
        state.serialize_field("version", &self.version)?;
        state.end()
    }
}

impl<'de> Deserialize<'de> for ClientHello {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let value = CborValue::deserialize(deserializer)?;
        let wire: ClientHelloWire = decode_wire(value).map_err(serde::de::Error::custom)?;
        require_type(&wire.type_field, "hello").map_err(serde::de::Error::custom)?;
        Ok(Self {
            version: wire.version,
        })
    }
}

impl Serialize for RequestEnvelope {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        let mut state = serializer.serialize_struct("RequestEnvelope", 4)?;
        state.serialize_field("type", "request")?;
        state.serialize_field("id", &self.id)?;
        state.serialize_field("target", &self.target)?;
        state.serialize_field("call", &OpaqueJson(&self.call))?;
        state.end()
    }
}

impl<'de> Deserialize<'de> for RequestEnvelope {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let value = CborValue::deserialize(deserializer)?;
        let wire: RequestWire = decode_wire(value).map_err(serde::de::Error::custom)?;
        require_type(&wire.type_field, "request").map_err(serde::de::Error::custom)?;
        Ok(Self {
            id: wire.id,
            target: wire.target,
            call: wire.call,
        })
    }
}

impl Serialize for CancelEnvelope {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        let mut state = serializer.serialize_struct("CancelEnvelope", 3)?;
        state.serialize_field("type", "cancel")?;
        state.serialize_field("id", &self.id)?;
        state.serialize_field("target", &self.target)?;
        state.end()
    }
}

impl<'de> Deserialize<'de> for CancelEnvelope {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let value = CborValue::deserialize(deserializer)?;
        let wire: CancelWire = decode_wire(value).map_err(serde::de::Error::custom)?;
        require_type(&wire.type_field, "cancel").map_err(serde::de::Error::custom)?;
        Ok(Self {
            id: wire.id,
            target: wire.target,
        })
    }
}

impl Serialize for ServerHello {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        let mut state = serializer.serialize_struct("ServerHello", 3)?;
        state.serialize_field("type", "hello")?;
        state.serialize_field("version", &self.version)?;
        state.serialize_field("serverId", &self.server_id)?;
        state.end()
    }
}

impl<'de> Deserialize<'de> for ServerHello {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let value = CborValue::deserialize(deserializer)?;
        let wire: ServerHelloWire = decode_wire(value).map_err(serde::de::Error::custom)?;
        require_type(&wire.type_field, "hello").map_err(serde::de::Error::custom)?;
        Ok(Self {
            version: wire.version,
            server_id: wire.server_id,
        })
    }
}

impl Serialize for ServerHelloError {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        let mut state = serializer.serialize_struct("ServerHelloError", 2)?;
        state.serialize_field("type", "hello_error")?;
        state.serialize_field("error", &self.error)?;
        state.end()
    }
}

impl<'de> Deserialize<'de> for ServerHelloError {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let value = CborValue::deserialize(deserializer)?;
        let wire: ServerHelloErrorWire = decode_wire(value).map_err(serde::de::Error::custom)?;
        require_type(&wire.type_field, "hello_error").map_err(serde::de::Error::custom)?;
        Ok(Self { error: wire.error })
    }
}

impl Serialize for ResponseEnvelope {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        match self {
            Self::Success { id, result } => {
                let field_count = if result.is_some() { 4 } else { 3 };
                let mut state = serializer.serialize_struct("ResponseEnvelope", field_count)?;
                state.serialize_field("type", "response")?;
                state.serialize_field("id", id)?;
                state.serialize_field("ok", &true)?;
                if let Some(result) = result {
                    state.serialize_field("result", &OpaqueJson(result))?;
                }
                state.end()
            }
            Self::Error { id, error } => {
                let mut state = serializer.serialize_struct("ResponseEnvelope", 4)?;
                state.serialize_field("type", "response")?;
                state.serialize_field("id", id)?;
                state.serialize_field("ok", &false)?;
                state.serialize_field("error", error)?;
                state.end()
            }
        }
    }
}

impl<'de> Deserialize<'de> for ResponseEnvelope {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        parse_response_value(CborValue::deserialize(deserializer)?)
            .map_err(serde::de::Error::custom)
    }
}

impl Serialize for ServiceEventEnvelope {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        let mut state = serializer.serialize_struct("ServiceEventEnvelope", 3)?;
        state.serialize_field("type", "service_update")?;
        state.serialize_field("subscriptionId", &self.subscription_id)?;
        state.serialize_field("update", &OpaqueJson(&self.update))?;
        state.end()
    }
}

impl<'de> Deserialize<'de> for ServiceEventEnvelope {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let value = CborValue::deserialize(deserializer)?;
        let wire: ServiceEventWire = decode_wire(value).map_err(serde::de::Error::custom)?;
        require_type(&wire.type_field, "service_update").map_err(serde::de::Error::custom)?;
        Ok(Self {
            subscription_id: wire.subscription_id,
            update: wire.update,
        })
    }
}

impl Serialize for AttachmentEnvelope {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        let mut state = serializer.serialize_struct("AttachmentEnvelope", 2)?;
        state.serialize_field("type", "attachment")?;
        state.serialize_field("attachment", &self.attachment)?;
        state.end()
    }
}

impl<'de> Deserialize<'de> for AttachmentEnvelope {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let value = CborValue::deserialize(deserializer)?;
        let wire: AttachmentWire = decode_wire(value).map_err(serde::de::Error::custom)?;
        require_type(&wire.type_field, "attachment").map_err(serde::de::Error::custom)?;
        Ok(Self {
            attachment: wire.attachment,
        })
    }
}

impl Serialize for ClientMessage {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        match self {
            Self::Hello { version } => {
                let mut state = serializer.serialize_struct("ClientHello", 2)?;
                state.serialize_field("type", "hello")?;
                state.serialize_field("version", version)?;
                state.end()
            }
            Self::Request { id, target, call } => {
                let mut state = serializer.serialize_struct("RequestEnvelope", 4)?;
                state.serialize_field("type", "request")?;
                state.serialize_field("id", id)?;
                state.serialize_field("target", target)?;
                state.serialize_field("call", &OpaqueJson(call))?;
                state.end()
            }
            Self::Cancel { id, target } => {
                let mut state = serializer.serialize_struct("CancelEnvelope", 3)?;
                state.serialize_field("type", "cancel")?;
                state.serialize_field("id", id)?;
                state.serialize_field("target", target)?;
                state.end()
            }
        }
    }
}

impl<'de> Deserialize<'de> for ClientMessage {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        parse_client_value(CborValue::deserialize(deserializer)?).map_err(serde::de::Error::custom)
    }
}

impl Serialize for ServerMessage {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        match self {
            Self::Hello { version, server_id } => {
                let mut state = serializer.serialize_struct("ServerHello", 3)?;
                state.serialize_field("type", "hello")?;
                state.serialize_field("version", version)?;
                state.serialize_field("serverId", server_id)?;
                state.end()
            }
            Self::HelloError { error } => {
                let mut state = serializer.serialize_struct("ServerHelloError", 2)?;
                state.serialize_field("type", "hello_error")?;
                state.serialize_field("error", error)?;
                state.end()
            }
            Self::Response { id, result } => {
                let field_count = if result.is_some() { 4 } else { 3 };
                let mut state = serializer.serialize_struct("ResponseEnvelope", field_count)?;
                state.serialize_field("type", "response")?;
                state.serialize_field("id", id)?;
                state.serialize_field("ok", &true)?;
                if let Some(result) = result {
                    state.serialize_field("result", &OpaqueJson(result))?;
                }
                state.end()
            }
            Self::ResponseError { id, error } => {
                let mut state = serializer.serialize_struct("ResponseEnvelope", 4)?;
                state.serialize_field("type", "response")?;
                state.serialize_field("id", id)?;
                state.serialize_field("ok", &false)?;
                state.serialize_field("error", error)?;
                state.end()
            }
            Self::ServiceUpdate {
                subscription_id,
                update,
            } => {
                let mut state = serializer.serialize_struct("ServiceEventEnvelope", 3)?;
                state.serialize_field("type", "service_update")?;
                state.serialize_field("subscriptionId", subscription_id)?;
                state.serialize_field("update", &OpaqueJson(update))?;
                state.end()
            }
            Self::Attachment { attachment } => {
                let mut state = serializer.serialize_struct("AttachmentEnvelope", 2)?;
                state.serialize_field("type", "attachment")?;
                state.serialize_field("attachment", attachment)?;
                state.end()
            }
        }
    }
}

impl<'de> Deserialize<'de> for ServerMessage {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        parse_server_value(CborValue::deserialize(deserializer)?).map_err(serde::de::Error::custom)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn server_ids_require_canonical_lowercase_uuidv4() {
        assert!(is_server_id("00000000-0000-4000-8000-000000000001"));
        assert!(!is_server_id("00000000-0000-7000-8000-000000000001"));
        assert!(!is_server_id("00000000-0000-4000-7000-000000000001"));
        assert!(!is_server_id("00000000-0000-4000-8000-00000000000A"));
    }

    #[test]
    fn response_result_distinguishes_absence_from_null() {
        let absent = CborValue::Map(vec![
            ("type".to_owned(), CborValue::Text("response".to_owned())),
            ("id".to_owned(), CborValue::Text("request-1".to_owned())),
            ("ok".to_owned(), CborValue::Bool(true)),
        ]);
        let explicit_null = CborValue::Map(vec![
            ("type".to_owned(), CborValue::Text("response".to_owned())),
            ("id".to_owned(), CborValue::Text("request-1".to_owned())),
            ("ok".to_owned(), CborValue::Bool(true)),
            ("result".to_owned(), CborValue::Null),
        ]);
        let absent = parse_response_value(absent).expect("valid response");
        let explicit_null = parse_response_value(explicit_null).expect("valid response");
        assert!(matches!(absent, ResponseEnvelope::Success { result: None, .. }));
        assert!(matches!(
            explicit_null,
            ResponseEnvelope::Success {
                result: Some(JsonValue::Null),
                ..
            }
        ));
    }

    #[test]
    fn known_envelopes_reject_unknown_fields() {
        let value = CborValue::Map(vec![
            ("type".to_owned(), CborValue::Text("request".to_owned())),
            ("id".to_owned(), CborValue::Text("request-1".to_owned())),
            (
                "target".to_owned(),
                CborValue::Map(vec![(
                    "serverId".to_owned(),
                    CborValue::Text("00000000-0000-4000-8000-000000000001".to_owned()),
                )]),
            ),
            ("call".to_owned(), CborValue::Map(Vec::new())),
            ("extra".to_owned(), CborValue::Bool(true)),
        ]);
        assert!(parse_client_value(value).is_err());
    }
}
