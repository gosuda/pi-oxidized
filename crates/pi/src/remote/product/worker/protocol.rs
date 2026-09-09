//! Session worker control-socket wire grammar.
//!
//! Mirrors `packages/coding-agent/src/experimental/session-worker.ts:51-188`
//! over the canonical Chord value domain.  Commands travel server-to-worker
//! and events worker-to-server as JSON records; every discriminant is the
//! exact source `type` literal and every member keeps its source spelling.
//!
//! Source strictness is preserved record-for-record: schemas built with the
//! source `StrictObject` helper (`additionalProperties: false` — the operation
//! request, both operation responses, the operation cancel, the scope, the
//! metadata, the options, and the embedded service call) reject unknown keys,
//! while the plain `Type.Object` variants (`shutdown`, `discover_workers`,
//! `session_demand`, and every event) tolerate them.  Optional members are
//! absence-only: a present `null` fails validation for `instance`, `code`,
//! `provider`, `model`, and `parentSessionId`, exactly like the TypeScript
//! `Type.Optional` schemas.  The one explicit-null channel is
//! `operation_result.result`, where an omitted member models the source's
//! `undefined` service result and a present `null` is a real JSON `null`
//! result.
//!
//! Identifiers stay [`JsString`] so source-admitted UTF-16 — including
//! unpaired surrogates — passes validation unchanged.  Integral members keep
//! binary64 semantics: `pid` uses [`JsInteger`] under the source
//! `minimum: 1` bound, while the unbounded `createdAt`/`storageVersion`
//! integers are validated whole-valued doubles (the source admits any
//! integer, including negatives and values beyond `2^53`), so no native
//! rollover domain is introduced.  The embedded service call reuses the
//! Chord service-call grammar ([`parse_service_call`]) instead of a second
//! validator.  No protocol version or member is added.
//!
//! Parsing consumes canonical [`JsonValue`] trees and serialization is the
//! explicit `into_json` conversion on each wire type — never an intermediate
//! foreign JSON tree.

use pi_agent::service::error::RemoteServiceErrorCode;
use pi_agent::service::value::{JsInteger, JsObject, JsString, JsonValue, is_json_value};
use pi_agent::service::wire::{ServiceCall, WireError, parse_service_call};
use thiserror::Error;

/// Environment variable carrying the worker's control socket address
/// (source `SESSION_WORKER_CONTROL_ADDRESS_ENV`).
pub const SESSION_WORKER_CONTROL_ADDRESS_ENV: &str = "PI_SESSION_WORKER_CONTROL_ADDRESS";
/// Environment variable carrying the worker's control-socket auth token
/// (source `SESSION_WORKER_CONTROL_TOKEN_ENV`).
pub const SESSION_WORKER_CONTROL_TOKEN_ENV: &str = "PI_SESSION_WORKER_CONTROL_TOKEN";
/// Environment variable carrying the base64url-encoded worker session key
/// (source `SESSION_WORKER_SESSION_KEY_ENV`).
pub const SESSION_WORKER_SESSION_KEY_ENV: &str = "PI_SESSION_WORKER_SESSION_KEY_BASE64";
/// Environment variable carrying the worker's coordinator peer id
/// (source `SESSION_WORKER_PEER_ID_ENV`).
pub const SESSION_WORKER_PEER_ID_ENV: &str = "PI_SESSION_WORKER_PEER_ID";

/// Malformed session worker wire input.
#[derive(Clone, Debug, Eq, Error, PartialEq)]
pub enum WorkerWireError {
    /// A record fails its field-set, identifier, or bound checks.
    #[error("Invalid {description}")]
    Invalid {
        /// Record description used in the error message.
        description: &'static str,
    },
    /// The embedded service call failed Chord service-call validation.
    #[error(transparent)]
    ServiceCall(#[from] WireError),
}

const fn invalid(description: &'static str) -> WorkerWireError {
    WorkerWireError::Invalid { description }
}

const COMMAND: &str = "session worker command";
const EVENT: &str = "session worker event";
const REQUEST: &str = "worker operation request";
const RESPONSE: &str = "worker operation response";
const SCOPE: &str = "worker operation scope";
const METADATA: &str = "session worker metadata";
const OPTIONS: &str = "session worker options";

/// Session identity metadata announced in `worker_ready`
/// (source `SessionWorkerMetadataSchema`, session-worker.ts:73-81).
#[derive(Clone, Debug, PartialEq)]
pub struct SessionWorkerMetadata {
    /// Session id; non-empty on the wire.
    pub id: JsString,
    /// Creation timestamp; any integral binary64 (source `Type.Integer()`).
    pub created_at: f64,
    /// Storage format version; any integral binary64.
    pub storage_version: f64,
    /// Working directory of the session.
    pub cwd: JsString,
    /// Session file path.
    pub path: JsString,
    /// Last modification time; any binary64 number.
    pub modified_at: f64,
    /// Parent session id; absent on the wire means the source `undefined`,
    /// and a present `null` is rejected.
    pub parent_session_id: Option<JsString>,
}

impl SessionWorkerMetadata {
    /// Converts to the canonical strict record tree.
    #[must_use]
    pub fn into_json(self) -> JsonValue {
        let mut object = JsObject::new();
        object.insert(key("id"), JsonValue::String(self.id));
        object.insert(key("createdAt"), JsonValue::Number(self.created_at));
        object.insert(
            key("storageVersion"),
            JsonValue::Number(self.storage_version),
        );
        object.insert(key("cwd"), JsonValue::String(self.cwd));
        object.insert(key("path"), JsonValue::String(self.path));
        object.insert(key("modifiedAt"), JsonValue::Number(self.modified_at));
        if let Some(parent) = self.parent_session_id {
            object.insert(key("parentSessionId"), JsonValue::String(parent));
        }
        JsonValue::Object(object)
    }
}

/// Validates a raw metadata record (strict field set).
///
/// # Errors
/// Returns [`WorkerWireError::Invalid`] for a non-record, a missing or
/// mistyped member, an unknown key, a non-integral `createdAt`/
/// `storageVersion`, or a present `null` `parentSessionId`.
pub fn parse_session_worker_metadata(
    value: &JsonValue,
) -> Result<SessionWorkerMetadata, WorkerWireError> {
    let record = record(value, METADATA)?;
    assert_keys(
        record,
        &[
            "id",
            "createdAt",
            "storageVersion",
            "cwd",
            "path",
            "modifiedAt",
        ],
        &["parentSessionId"],
        METADATA,
    )?;
    let Some(id) = id_field(record, "id") else {
        return Err(invalid(METADATA));
    };
    let created_at = integer_field(record, "createdAt", METADATA)?;
    let storage_version = integer_field(record, "storageVersion", METADATA)?;
    let (Some(cwd), Some(path)) = (string_field(record, "cwd"), string_field(record, "path"))
    else {
        return Err(invalid(METADATA));
    };
    let modified_at = number_field(record, "modifiedAt", METADATA)?;
    let parent_session_id = optional_string(record, "parentSessionId")?;
    Ok(SessionWorkerMetadata {
        id: id.clone(),
        created_at,
        storage_version,
        cwd: cwd.clone(),
        path: path.clone(),
        modified_at,
        parent_session_id: parent_session_id.cloned(),
    })
}

/// Worker process options passed as the single command-line argument
/// (source `SessionWorkerOptionsSchema`, session-worker.ts:83-90).
#[derive(Clone, Debug, PartialEq)]
pub struct SessionWorkerOptions {
    /// Root directory holding the session store; non-empty.
    pub session_dir: JsString,
    /// Session identity metadata.
    pub metadata: SessionWorkerMetadata,
    /// Explicit provider selection; absent means source `undefined`.
    pub provider: Option<JsString>,
    /// Explicit model selection; absent means source `undefined`.
    pub model: Option<JsString>,
    /// Plugin facet manifests to load; every entry non-empty.
    pub plugin_manifest_paths: Vec<JsString>,
}

impl SessionWorkerOptions {
    /// Converts to the canonical strict record tree.
    #[must_use]
    pub fn into_json(self) -> JsonValue {
        let mut object = JsObject::new();
        object.insert(key("sessionDir"), JsonValue::String(self.session_dir));
        object.insert(key("metadata"), self.metadata.into_json());
        if let Some(provider) = self.provider {
            object.insert(key("provider"), JsonValue::String(provider));
        }
        if let Some(model) = self.model {
            object.insert(key("model"), JsonValue::String(model));
        }
        object.insert(
            key("pluginManifestPaths"),
            JsonValue::Array(
                self.plugin_manifest_paths
                    .into_iter()
                    .map(JsonValue::String)
                    .collect(),
            ),
        );
        JsonValue::Object(object)
    }
}

/// Validates a raw options record (strict field set).  The additional
/// path-absoluteness and provider/model pairing checks from
/// `runSessionWorkerWithHarness` belong to the worker entrypoint, not the
/// wire grammar.
///
/// # Errors
/// Returns [`WorkerWireError::Invalid`] for a non-record, a missing or
/// mistyped member, an unknown key, an empty identifier where the source
/// demands `minLength: 1`, or a present `null` optional.
pub fn parse_session_worker_options(
    value: &JsonValue,
) -> Result<SessionWorkerOptions, WorkerWireError> {
    let options = record(value, OPTIONS)?;
    assert_keys(
        options,
        &["sessionDir", "metadata", "pluginManifestPaths"],
        &["provider", "model"],
        OPTIONS,
    )?;
    let Some(session_dir) = id_field(options, "sessionDir") else {
        return Err(invalid(OPTIONS));
    };
    let Some(metadata) = field(options, "metadata") else {
        return Err(invalid(OPTIONS));
    };
    let metadata = parse_session_worker_metadata(metadata)?;
    let provider = optional_id(options, "provider")?;
    let model = optional_id(options, "model")?;
    let Some(paths) = array_field(options, "pluginManifestPaths") else {
        return Err(invalid(OPTIONS));
    };
    if paths.iter().any(|value| id_value(value).is_none()) {
        return Err(invalid(OPTIONS));
    }
    let plugin_manifest_paths = paths
        .iter()
        .filter_map(|value| match value {
            JsonValue::String(path) => Some(path.clone()),
            _ => None,
        })
        .collect();
    Ok(SessionWorkerOptions {
        session_dir: session_dir.clone(),
        metadata,
        provider: provider.cloned(),
        model: model.cloned(),
        plugin_manifest_paths,
    })
}

/// The server connection and attachment a demand or operation belongs to
/// (source `WorkerOperationScopeSchema`, session-worker.ts:92-96, aliasing
/// the services `WorkerServiceScope`).  Fields admit any string, including
/// empty ones — the source schema carries no `minLength`.
#[derive(Clone, Debug, Eq, Hash, PartialEq)]
pub struct WorkerOperationScope {
    /// Server generation connection id.
    pub server_connection_id: JsString,
    /// Attachment id within that connection.
    pub attachment_id: JsString,
}

impl WorkerOperationScope {
    /// Converts to the canonical strict record tree.
    #[must_use]
    pub fn into_json(self) -> JsonValue {
        let mut object = JsObject::new();
        object.insert(
            key("serverConnectionId"),
            JsonValue::String(self.server_connection_id),
        );
        object.insert(key("attachmentId"), JsonValue::String(self.attachment_id));
        JsonValue::Object(object)
    }
}

/// Validates a raw scope record (strict field set, any string values).
///
/// # Errors
/// Returns [`WorkerWireError::Invalid`] for a non-record, missing or
/// non-string members, or an unknown key.
pub fn parse_worker_operation_scope(
    value: &JsonValue,
) -> Result<WorkerOperationScope, WorkerWireError> {
    let scope = record(value, SCOPE)?;
    assert_keys(scope, &["serverConnectionId", "attachmentId"], &[], SCOPE)?;
    let (Some(server_connection_id), Some(attachment_id)) = (
        string_field(scope, "serverConnectionId"),
        string_field(scope, "attachmentId"),
    ) else {
        return Err(invalid(SCOPE));
    };
    Ok(WorkerOperationScope {
        server_connection_id: server_connection_id.clone(),
        attachment_id: attachment_id.clone(),
    })
}

/// One service invocation routed to the worker
/// (source `WorkerOperationRequestSchema`, session-worker.ts:98-104).
#[derive(Clone, Debug, PartialEq)]
pub struct WorkerOperationRequest {
    /// Correlation id shared with the response; non-empty.
    pub request_id: JsString,
    /// Demand scope of the invocation.
    pub scope: WorkerOperationScope,
    /// Service call, validated by the Chord service-call grammar.
    pub call: ServiceCall,
}

impl WorkerOperationRequest {
    /// Converts to the canonical strict record tree with the `operation` tag.
    #[must_use]
    pub fn into_json(self) -> JsonValue {
        let mut object = JsObject::new();
        object.insert(key("type"), string("operation"));
        object.insert(key("requestId"), JsonValue::String(self.request_id));
        object.insert(key("scope"), self.scope.into_json());
        object.insert(key("call"), self.call.into_json());
        JsonValue::Object(object)
    }
}

/// Validates a raw operation request (strict field set).  The embedded call
/// is validated by [`parse_service_call`], whose grammar equals the source
/// `ServiceCallSchema`.
///
/// # Errors
/// Returns [`WorkerWireError::Invalid`] for a non-record, a missing or
/// mistyped member, an unknown key, or an empty `requestId`; returns
/// [`WorkerWireError::ServiceCall`] when the call fails service-call
/// validation.
pub fn parse_worker_operation_request(
    value: &JsonValue,
) -> Result<WorkerOperationRequest, WorkerWireError> {
    let request = record(value, REQUEST)?;
    let Some(tag) = string_field(request, "type") else {
        return Err(invalid(REQUEST));
    };
    if !same_text(tag, "operation") {
        return Err(invalid(REQUEST));
    }
    assert_keys(
        request,
        &["type", "requestId", "scope", "call"],
        &[],
        REQUEST,
    )?;
    let Some(request_id) = id_field(request, "requestId") else {
        return Err(invalid(REQUEST));
    };
    let Some(scope) = field(request, "scope") else {
        return Err(invalid(REQUEST));
    };
    let scope = parse_worker_operation_scope(scope)?;
    let Some(call) = field(request, "call") else {
        return Err(invalid(REQUEST));
    };
    let call = parse_service_call(call)?;
    Ok(WorkerOperationRequest {
        request_id: request_id.clone(),
        scope,
        call,
    })
}

/// The response to one operation request
/// (source `WorkerOperationResponseSchema`, session-worker.ts:106-121).
#[derive(Clone, Debug, PartialEq)]
pub enum WorkerOperationResponse {
    /// `operation_result`: `result` is absent when the service returned the
    /// source `undefined`, and `Some(JsonValue::Null)` is an explicit JSON
    /// `null` result.
    OperationResult {
        /// Correlation id; non-empty.
        request_id: JsString,
        /// Demand scope of the completed invocation.
        scope: WorkerOperationScope,
        /// Strict-JSON service result; `None` omits the member entirely.
        result: Option<JsonValue>,
    },
    /// `operation_error`: `code` is absent when the failure carries no
    /// remote-service code, and `message` is the diagnostic text.
    OperationError {
        /// Correlation id; non-empty.
        request_id: JsString,
        /// Demand scope of the failed invocation.
        scope: WorkerOperationScope,
        /// One of the eight remote-service protocol codes when present.
        code: Option<RemoteServiceErrorCode>,
        /// Diagnostic message.
        message: JsString,
    },
}

impl WorkerOperationResponse {
    /// Converts to the canonical strict record tree with the variant tag.
    #[must_use]
    pub fn into_json(self) -> JsonValue {
        match self {
            Self::OperationResult {
                request_id,
                scope,
                result,
            } => {
                let mut object = JsObject::new();
                object.insert(key("type"), string("operation_result"));
                object.insert(key("requestId"), JsonValue::String(request_id));
                object.insert(key("scope"), scope.into_json());
                if let Some(result) = result {
                    object.insert(key("result"), result);
                }
                JsonValue::Object(object)
            }
            Self::OperationError {
                request_id,
                scope,
                code,
                message,
            } => {
                let mut object = JsObject::new();
                object.insert(key("type"), string("operation_error"));
                object.insert(key("requestId"), JsonValue::String(request_id));
                object.insert(key("scope"), scope.into_json());
                if let Some(code) = code {
                    object.insert(key("code"), string(code.as_str()));
                }
                object.insert(key("message"), JsonValue::String(message));
                JsonValue::Object(object)
            }
        }
    }
}

/// Validates a raw operation response (both variants are strict records).
///
/// # Errors
/// Returns [`WorkerWireError::Invalid`] for a non-record, an unknown tag, a
/// missing or mistyped member, an unknown key, an empty `requestId`, a
/// `code` outside the eight protocol codes (including a present `null`), or
/// a `result` that is not strict JSON.
pub fn parse_worker_operation_response(
    value: &JsonValue,
) -> Result<WorkerOperationResponse, WorkerWireError> {
    let response = record(value, RESPONSE)?;
    let Some(tag) = string_field(response, "type") else {
        return Err(invalid(RESPONSE));
    };
    if same_text(tag, "operation_result") {
        assert_keys(
            response,
            &["type", "requestId", "scope"],
            &["result"],
            RESPONSE,
        )?;
        let (Some(request_id), Some(scope)) =
            (id_field(response, "requestId"), field(response, "scope"))
        else {
            return Err(invalid(RESPONSE));
        };
        let scope = parse_worker_operation_scope(scope)?;
        let result = match field(response, "result") {
            // A present value — including an explicit `null` — is kept; only
            // a genuinely absent member models the source `undefined`.
            Some(result) if is_json_value(result) => Some(result.clone()),
            Some(_) => return Err(invalid(RESPONSE)),
            None => None,
        };
        Ok(WorkerOperationResponse::OperationResult {
            request_id: request_id.clone(),
            scope,
            result,
        })
    } else if same_text(tag, "operation_error") {
        assert_keys(
            response,
            &["type", "requestId", "scope", "message"],
            &["code"],
            RESPONSE,
        )?;
        let (Some(request_id), Some(scope), Some(message)) = (
            id_field(response, "requestId"),
            field(response, "scope"),
            string_field(response, "message"),
        ) else {
            return Err(invalid(RESPONSE));
        };
        let scope = parse_worker_operation_scope(scope)?;
        let code = match field(response, "code") {
            Some(raw) => match error_code(raw) {
                Some(code) => Some(code),
                None => return Err(invalid(RESPONSE)),
            },
            None => None,
        };
        Ok(WorkerOperationResponse::OperationError {
            request_id: request_id.clone(),
            scope,
            code,
            message: message.clone(),
        })
    } else {
        Err(invalid(RESPONSE))
    }
}

/// One `session_demand` command payload
/// (source `SessionWorkerCommandSchema` member, session-worker.ts:126-132).
#[derive(Clone, Debug, PartialEq)]
pub struct SessionDemand {
    /// Server generation connection id; any string.
    pub server_connection_id: JsString,
    /// Correlation id for the applied/rejected event; any string.
    pub request_id: JsString,
    /// Attachment id within the connection; any string.
    pub attachment_id: JsString,
    /// Whether the attachment is demanded (`true`) or released (`false`).
    pub attached: bool,
}

impl SessionDemand {
    /// Converts to the canonical `session_demand` record tree.
    #[must_use]
    pub fn into_json(self) -> JsonValue {
        let mut object = JsObject::new();
        object.insert(key("type"), string("session_demand"));
        object.insert(
            key("serverConnectionId"),
            JsonValue::String(self.server_connection_id),
        );
        object.insert(key("requestId"), JsonValue::String(self.request_id));
        object.insert(key("attachmentId"), JsonValue::String(self.attachment_id));
        object.insert(key("attached"), JsonValue::Bool(self.attached));
        JsonValue::Object(object)
    }
}

/// One `operation_cancel` command payload
/// (source session-worker.ts:134-138).
#[derive(Clone, Debug, PartialEq)]
pub struct OperationCancel {
    /// Correlation id of the request to cancel; non-empty.
    pub request_id: JsString,
    /// Demand scope of the cancelled request.
    pub scope: WorkerOperationScope,
}

impl OperationCancel {
    /// Converts to the canonical strict record tree.
    #[must_use]
    pub fn into_json(self) -> JsonValue {
        let mut object = JsObject::new();
        object.insert(key("type"), string("operation_cancel"));
        object.insert(key("requestId"), JsonValue::String(self.request_id));
        object.insert(key("scope"), self.scope.into_json());
        JsonValue::Object(object)
    }
}

/// One server-to-worker control command
/// (source `SessionWorkerCommandSchema`, session-worker.ts:123-140).
///
/// `Shutdown`, `DiscoverWorkers`, and `SessionDemand` keep the source's
/// non-strict records (unknown JSON keys are tolerated on parse); the
/// operation variants are strict.
#[derive(Clone, Debug, PartialEq)]
pub enum SessionWorkerCommand {
    /// `shutdown`: retire and exit.
    Shutdown,
    /// `discover_workers`: re-announce `worker_ready`.
    DiscoverWorkers,
    /// `session_demand`: attach or release one attachment.
    SessionDemand(SessionDemand),
    /// `operation`: invoke one service call.
    Operation(WorkerOperationRequest),
    /// `operation_cancel`: cancel one in-flight operation.
    OperationCancel(OperationCancel),
}

impl SessionWorkerCommand {
    /// Converts to the canonical record tree with the variant tag.
    #[must_use]
    pub fn into_json(self) -> JsonValue {
        match self {
            Self::Shutdown => {
                let mut object = JsObject::new();
                object.insert(key("type"), string("shutdown"));
                JsonValue::Object(object)
            }
            Self::DiscoverWorkers => {
                let mut object = JsObject::new();
                object.insert(key("type"), string("discover_workers"));
                JsonValue::Object(object)
            }
            Self::SessionDemand(demand) => demand.into_json(),
            Self::Operation(request) => request.into_json(),
            Self::OperationCancel(cancel) => cancel.into_json(),
        }
    }
}

/// Validates one raw command record.
///
/// # Errors
/// Returns [`WorkerWireError::Invalid`] for a non-record, an unknown `type`
/// tag, a missing or mistyped member, or — for the strict variants — an
/// unknown key, an empty `requestId`, or a malformed call/scope.
pub fn parse_session_worker_command(
    value: &JsonValue,
) -> Result<SessionWorkerCommand, WorkerWireError> {
    let command = record(value, COMMAND)?;
    let Some(tag) = string_field(command, "type") else {
        return Err(invalid(COMMAND));
    };
    if same_text(tag, "shutdown") {
        return Ok(SessionWorkerCommand::Shutdown);
    }
    if same_text(tag, "discover_workers") {
        return Ok(SessionWorkerCommand::DiscoverWorkers);
    }
    if same_text(tag, "session_demand") {
        let (Some(server_connection_id), Some(request_id), Some(attachment_id), Some(attached)) = (
            string_field(command, "serverConnectionId"),
            string_field(command, "requestId"),
            string_field(command, "attachmentId"),
            bool_field(command, "attached"),
        ) else {
            return Err(invalid(COMMAND));
        };
        return Ok(SessionWorkerCommand::SessionDemand(SessionDemand {
            server_connection_id: server_connection_id.clone(),
            request_id: request_id.clone(),
            attachment_id: attachment_id.clone(),
            attached,
        }));
    }
    if same_text(tag, "operation") {
        return parse_worker_operation_request(value).map(SessionWorkerCommand::Operation);
    }
    if same_text(tag, "operation_cancel") {
        assert_keys(command, &["type", "requestId", "scope"], &[], COMMAND)?;
        let (Some(request_id), Some(scope)) =
            (id_field(command, "requestId"), field(command, "scope"))
        else {
            return Err(invalid(COMMAND));
        };
        let scope = parse_worker_operation_scope(scope)?;
        return Ok(SessionWorkerCommand::OperationCancel(OperationCancel {
            request_id: request_id.clone(),
            scope,
        }));
    }
    Err(invalid(COMMAND))
}

/// One worker-to-server event
/// (source `SessionWorkerEventSchema`, session-worker.ts:142-188).  Every
/// variant keeps the source's non-strict records (unknown JSON keys are
/// tolerated on parse); nested schemas stay strict.
#[derive(Clone, Debug, PartialEq)]
pub enum SessionWorkerEvent {
    /// `worker_ready`: announces the worker to the server.
    WorkerReady {
        /// Control-socket auth token.
        token: JsString,
        /// Base64-decoded session key the token authenticates.
        session_key: JsString,
        /// Session id.
        session_id: JsString,
        /// Worker process id; an integral binary64 with the source
        /// `minimum: 1` bound.
        pid: JsInteger,
        /// Session identity metadata.
        metadata: SessionWorkerMetadata,
        /// Plugin facet manifests; every entry non-empty.
        plugin_manifest_paths: Vec<JsString>,
    },
    /// `worker_failed`: reports a fatal startup or runtime failure.
    WorkerFailed {
        /// Control-socket auth token.
        token: JsString,
        /// Base64-decoded session key.
        session_key: JsString,
        /// Diagnostic message.
        message: JsString,
    },
    /// `demand_applied`: acknowledges an applied `session_demand`.
    DemandApplied {
        /// Control-socket auth token.
        token: JsString,
        /// Base64-decoded session key.
        session_key: JsString,
        /// Correlation id from the command; any string.
        request_id: JsString,
        /// Attachment id from the command; any string.
        attachment_id: JsString,
        /// Attachment state that was applied.
        attached: bool,
    },
    /// `demand_rejected`: reports a rejected `session_demand`.
    DemandRejected {
        /// Control-socket auth token.
        token: JsString,
        /// Base64-decoded session key.
        session_key: JsString,
        /// Correlation id from the command; any string.
        request_id: JsString,
        /// Rejection reason.
        message: JsString,
    },
    /// `operation_response`: completes one operation request.
    OperationResponse {
        /// Control-socket auth token.
        token: JsString,
        /// Base64-decoded session key.
        session_key: JsString,
        /// The operation result or error.
        response: WorkerOperationResponse,
    },
    /// `service_update`: pushes one provider update for a subscription.
    ServiceUpdate {
        /// Control-socket auth token.
        token: JsString,
        /// Base64-decoded session key.
        session_key: JsString,
        /// Demand scope of the subscription.
        scope: WorkerOperationScope,
        /// Subscription id; non-empty.
        subscription_id: JsString,
        /// Raw provider update; any strict-JSON value, passed through.
        update: JsonValue,
    },
}

impl SessionWorkerEvent {
    /// Converts to the canonical record tree with the variant tag.
    #[must_use]
    pub fn into_json(self) -> JsonValue {
        match self {
            Self::WorkerReady {
                token,
                session_key,
                session_id,
                pid,
                metadata,
                plugin_manifest_paths,
            } => {
                let mut object = JsObject::new();
                object.insert(key("type"), string("worker_ready"));
                object.insert(key("token"), JsonValue::String(token));
                object.insert(key("sessionKey"), JsonValue::String(session_key));
                object.insert(key("sessionId"), JsonValue::String(session_id));
                object.insert(key("pid"), JsonValue::Number(pid.as_f64()));
                object.insert(key("metadata"), metadata.into_json());
                object.insert(
                    key("pluginManifestPaths"),
                    JsonValue::Array(
                        plugin_manifest_paths
                            .into_iter()
                            .map(JsonValue::String)
                            .collect(),
                    ),
                );
                JsonValue::Object(object)
            }
            Self::WorkerFailed {
                token,
                session_key,
                message,
            } => {
                let mut object = tagged("worker_failed", &token, &session_key);
                object.insert(key("message"), JsonValue::String(message));
                JsonValue::Object(object)
            }
            Self::DemandApplied {
                token,
                session_key,
                request_id,
                attachment_id,
                attached,
            } => {
                let mut object = tagged("demand_applied", &token, &session_key);
                object.insert(key("requestId"), JsonValue::String(request_id));
                object.insert(key("attachmentId"), JsonValue::String(attachment_id));
                object.insert(key("attached"), JsonValue::Bool(attached));
                JsonValue::Object(object)
            }
            Self::DemandRejected {
                token,
                session_key,
                request_id,
                message,
            } => {
                let mut object = tagged("demand_rejected", &token, &session_key);
                object.insert(key("requestId"), JsonValue::String(request_id));
                object.insert(key("message"), JsonValue::String(message));
                JsonValue::Object(object)
            }
            Self::OperationResponse {
                token,
                session_key,
                response,
            } => {
                let mut object = tagged("operation_response", &token, &session_key);
                object.insert(key("response"), response.into_json());
                JsonValue::Object(object)
            }
            Self::ServiceUpdate {
                token,
                session_key,
                scope,
                subscription_id,
                update,
            } => {
                let mut object = tagged("service_update", &token, &session_key);
                object.insert(key("scope"), scope.into_json());
                object.insert(key("subscriptionId"), JsonValue::String(subscription_id));
                object.insert(key("update"), update);
                JsonValue::Object(object)
            }
        }
    }
}

#[expect(
    clippy::too_many_lines,
    reason = "mirrors the source TypeScript schema validator (session-worker.ts:142-188) and is kept as one exhaustive match over the wire type tag"
)]
/// Validates one raw event record.
///
/// # Errors
/// Returns [`WorkerWireError::Invalid`] for a non-record, an unknown `type`
/// tag, a missing or mistyped member, a non-integral or sub-one `pid`, an
/// invalid nested metadata/response/scope, or an empty `subscriptionId` or
/// manifest path.
pub fn parse_session_worker_event(
    value: &JsonValue,
) -> Result<SessionWorkerEvent, WorkerWireError> {
    let event = record(value, EVENT)?;
    let Some(tag) = string_field(event, "type") else {
        return Err(invalid(EVENT));
    };
    if same_text(tag, "worker_ready") {
        let (Some(token), Some(session_key), Some(session_id)) = (
            string_field(event, "token"),
            string_field(event, "sessionKey"),
            string_field(event, "sessionId"),
        ) else {
            return Err(invalid(EVENT));
        };
        let pid = integer_member(event, "pid", 1.0, EVENT)?;
        let pid = JsInteger::new(pid).map_err(|_| invalid(EVENT))?;
        let Some(metadata) = field(event, "metadata") else {
            return Err(invalid(EVENT));
        };
        let metadata = parse_session_worker_metadata(metadata)?;
        let Some(paths) = array_field(event, "pluginManifestPaths") else {
            return Err(invalid(EVENT));
        };
        if paths.iter().any(|value| id_value(value).is_none()) {
            return Err(invalid(EVENT));
        }
        let plugin_manifest_paths = paths
            .iter()
            .filter_map(|value| match value {
                JsonValue::String(path) => Some(path.clone()),
                _ => None,
            })
            .collect();
        return Ok(SessionWorkerEvent::WorkerReady {
            token: token.clone(),
            session_key: session_key.clone(),
            session_id: session_id.clone(),
            pid,
            metadata,
            plugin_manifest_paths,
        });
    }
    if same_text(tag, "worker_failed") {
        let (Some(token), Some(session_key), Some(message)) = (
            string_field(event, "token"),
            string_field(event, "sessionKey"),
            string_field(event, "message"),
        ) else {
            return Err(invalid(EVENT));
        };
        return Ok(SessionWorkerEvent::WorkerFailed {
            token: token.clone(),
            session_key: session_key.clone(),
            message: message.clone(),
        });
    }
    if same_text(tag, "demand_applied") {
        let (Some(token), Some(session_key), Some(request_id), Some(attachment_id), Some(attached)) = (
            string_field(event, "token"),
            string_field(event, "sessionKey"),
            string_field(event, "requestId"),
            string_field(event, "attachmentId"),
            bool_field(event, "attached"),
        ) else {
            return Err(invalid(EVENT));
        };
        return Ok(SessionWorkerEvent::DemandApplied {
            token: token.clone(),
            session_key: session_key.clone(),
            request_id: request_id.clone(),
            attachment_id: attachment_id.clone(),
            attached,
        });
    }
    if same_text(tag, "demand_rejected") {
        let (Some(token), Some(session_key), Some(request_id), Some(message)) = (
            string_field(event, "token"),
            string_field(event, "sessionKey"),
            string_field(event, "requestId"),
            string_field(event, "message"),
        ) else {
            return Err(invalid(EVENT));
        };
        return Ok(SessionWorkerEvent::DemandRejected {
            token: token.clone(),
            session_key: session_key.clone(),
            request_id: request_id.clone(),
            message: message.clone(),
        });
    }
    if same_text(tag, "operation_response") {
        let (Some(token), Some(session_key), Some(response)) = (
            string_field(event, "token"),
            string_field(event, "sessionKey"),
            field(event, "response"),
        ) else {
            return Err(invalid(EVENT));
        };
        let response = parse_worker_operation_response(response)?;
        return Ok(SessionWorkerEvent::OperationResponse {
            token: token.clone(),
            session_key: session_key.clone(),
            response,
        });
    }
    if same_text(tag, "service_update") {
        let (Some(token), Some(session_key), Some(scope), Some(subscription_id), Some(update)) = (
            string_field(event, "token"),
            string_field(event, "sessionKey"),
            field(event, "scope"),
            id_field(event, "subscriptionId"),
            field(event, "update"),
        ) else {
            return Err(invalid(EVENT));
        };
        if !is_json_value(update) {
            return Err(invalid(EVENT));
        }
        let scope = parse_worker_operation_scope(scope)?;
        return Ok(SessionWorkerEvent::ServiceUpdate {
            token: token.clone(),
            session_key: session_key.clone(),
            scope,
            subscription_id: subscription_id.clone(),
            update: update.clone(),
        });
    }
    Err(invalid(EVENT))
}

/// Builds a canonical object key from a source field name.
fn key(name: &str) -> JsString {
    JsString::from_utf8(name)
}

/// Builds a canonical string value from a fixed ASCII discriminant.
fn string(value: &str) -> JsonValue {
    JsonValue::String(JsString::from_utf8(value))
}

/// The leading `type`/`token`/`sessionKey` members shared by every event.
fn tagged(tag: &str, token: &JsString, session_key: &JsString) -> JsObject {
    let mut object = JsObject::new();
    object.insert(key("type"), string(tag));
    object.insert(key("token"), JsonValue::String(token.clone()));
    object.insert(key("sessionKey"), JsonValue::String(session_key.clone()));
    object
}

/// Rejects non-object values, including arrays and `null`.
fn record<'a>(
    value: &'a JsonValue,
    description: &'static str,
) -> Result<&'a JsObject, WorkerWireError> {
    match value {
        JsonValue::Object(object) => Ok(object),
        _ => Err(invalid(description)),
    }
}

/// Enforces a required field set and, optionally, an additional set of
/// absence-only optional fields.
fn assert_keys(
    object: &JsObject,
    required: &[&str],
    optional: &[&str],
    description: &'static str,
) -> Result<(), WorkerWireError> {
    if required.iter().any(|name| field(object, name).is_none()) {
        return Err(invalid(description));
    }
    if object.keys().any(|name| {
        !required
            .iter()
            .chain(optional.iter())
            .any(|allowed| same_text(name, allowed))
    }) {
        return Err(invalid(description));
    }
    Ok(())
}

/// Compares a canonical UTF-16 string with an ASCII/UTF-8 source literal
/// without lossy replacement-character conversion.
fn same_text(value: &JsString, expected: &str) -> bool {
    value.as_utf16().iter().copied().eq(expected.encode_utf16())
}

/// Finds a field by its source JSON spelling without allocating a key.
fn field<'a>(object: &'a JsObject, name: &str) -> Option<&'a JsonValue> {
    object
        .iter()
        .find_map(|(key, value)| same_text(key, name).then_some(value))
}

fn string_field<'a>(object: &'a JsObject, name: &str) -> Option<&'a JsString> {
    field(object, name).and_then(|value| match value {
        JsonValue::String(value) => Some(value),
        _ => None,
    })
}

fn bool_field(object: &JsObject, name: &str) -> Option<bool> {
    field(object, name).and_then(JsonValue::as_bool)
}

fn array_field<'a>(object: &'a JsObject, name: &str) -> Option<&'a Vec<JsonValue>> {
    field(object, name).and_then(JsonValue::as_array)
}

fn id_value(value: &JsonValue) -> Option<&JsString> {
    match value {
        JsonValue::String(value) if !value.as_utf16().is_empty() => Some(value),
        _ => None,
    }
}

fn id_field<'a>(object: &'a JsObject, name: &str) -> Option<&'a JsString> {
    field(object, name).and_then(id_value)
}

/// Reads an absence-only optional string.  A present `null` or non-string is
/// invalid; only an absent field returns `Ok(None)`.
fn optional_string<'a>(
    object: &'a JsObject,
    name: &str,
) -> Result<Option<&'a JsString>, WorkerWireError> {
    match field(object, name) {
        Some(JsonValue::String(value)) => Ok(Some(value)),
        Some(_) => Err(invalid(METADATA)),
        None => Ok(None),
    }
}

/// Reads an absence-only non-empty optional identifier.
fn optional_id<'a>(
    object: &'a JsObject,
    name: &str,
) -> Result<Option<&'a JsString>, WorkerWireError> {
    match field(object, name) {
        Some(value) => id_value(value).map(Some).ok_or_else(|| invalid(OPTIONS)),
        None => Ok(None),
    }
}

fn number_field(
    object: &JsObject,
    name: &str,
    description: &'static str,
) -> Result<f64, WorkerWireError> {
    match field(object, name) {
        Some(JsonValue::Number(value)) if value.is_finite() => Ok(*value),
        _ => Err(invalid(description)),
    }
}

/// Validates `Type.Integer({ minimum })`: finite, whole-valued binary64 with
/// the source minimum, without imposing a native integer-width limit.
fn integer_member(
    object: &JsObject,
    name: &str,
    minimum: f64,
    description: &'static str,
) -> Result<f64, WorkerWireError> {
    let value = number_field(object, name, description)?;
    if value < minimum || value.fract() != 0.0 {
        return Err(invalid(description));
    }
    Ok(value)
}

fn integer_field(
    object: &JsObject,
    name: &str,
    description: &'static str,
) -> Result<f64, WorkerWireError> {
    let value = number_field(object, name, description)?;
    if value.fract() != 0.0 {
        return Err(invalid(description));
    }
    Ok(value)
}

fn error_code(value: &JsonValue) -> Option<RemoteServiceErrorCode> {
    let JsonValue::String(value) = value else {
        return None;
    };
    [
        RemoteServiceErrorCode::ServiceNotAllowed,
        RemoteServiceErrorCode::ServiceNotFound,
        RemoteServiceErrorCode::ServiceModeMismatch,
        RemoteServiceErrorCode::ServiceMemberNotFound,
        RemoteServiceErrorCode::ServiceMemberMismatch,
        RemoteServiceErrorCode::ServiceInstanceNotFound,
        RemoteServiceErrorCode::ServiceStaleInstance,
        RemoteServiceErrorCode::ServiceInvalidValue,
    ]
    .into_iter()
    .find(|code| same_text(value, code.as_str()))
}
