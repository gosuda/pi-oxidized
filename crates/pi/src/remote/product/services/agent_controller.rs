//! Agent-lane command service contracts.
//!
//! Source mapping: `experimental/services/agent-controller.ts`. Lane
//! operations and error classification remain provider-owned; this module
//! carries only the source-shaped request and response values.

use pi_agent::service::error::ServiceError;
use pi_agent::service::value::JsonValue;

use super::{
    ProductJsonConvert, array, bool_value, json_object, nullable, nullable_string, object,
    required, string,
};

/// Chord service identifier for the worker-owned agent controller.
pub const AGENT_CONTROLLER_ID: &str = "pi.agent-controller";

/// Wire method that asks `pi.agent-controller` to submit a prompt to the lane.
pub const AGENT_CONTROLLER_PROMPT_MEMBER: &str = "prompt";
/// Wire method that asks `pi.agent-controller` to abort a running operation.
pub const AGENT_CONTROLLER_REQUEST_ABORT_MEMBER: &str = "requestAbort";
/// Wire method that asks `pi.agent-controller` to queue a steering prompt.
pub const AGENT_CONTROLLER_STEER_MEMBER: &str = "steer";
/// Wire method that asks `pi.agent-controller` to queue a follow-up prompt.
pub const AGENT_CONTROLLER_FOLLOW_UP_MEMBER: &str = "followUp";
/// Wire method that asks `pi.agent-controller` to enqueue a prompt for the next run.
pub const AGENT_CONTROLLER_NEXT_RUN_MEMBER: &str = "nextRun";
/// Wire method that asks `pi.agent-controller` to cancel a queued entry.
pub const AGENT_CONTROLLER_CANCEL_QUEUED_MEMBER: &str = "cancelQueued";
/// Wire method that asks `pi.agent-controller` to resume the lane.
pub const AGENT_CONTROLLER_RESUME_MEMBER: &str = "resume";
/// Wire method that asks `pi.agent-controller` to compact the lane with custom instructions.
pub const AGENT_CONTROLLER_COMPACT_MEMBER: &str = "compact";
/// Wire method that asks `pi.agent-controller` to navigate the lane tree.
pub const AGENT_CONTROLLER_NAVIGATE_MEMBER: &str = "navigate";

/// One image supplied with a prompt.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AgentPromptImage {
    /// Source literal discriminator. It is `"image"` for this service.
    pub r#type: String,
    /// Base64-encoded image data.
    pub data: String,
    /// MIME type of the image.
    pub mime_type: String,
}

/// Prompt request accepted by prompt, steer, follow-up, and next-run methods.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AgentPromptRequest {
    /// User message text.
    pub message: String,
    /// Images, or `null` when no images were supplied.
    pub images: Option<Vec<AgentPromptImage>>,
}

/// Stable error payload returned in an accepted or rejected operation result.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AgentOperationError {
    /// Stable source error code.
    pub code: String,
    /// Human-readable source message.
    pub message: String,
}

/// Accepted/rejected response for operations that receive an operation id.
///
/// The source union is represented without adding validation rules: callers
/// preserve the `accepted` flag and nullable fields exactly as received.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AgentOperationResponse {
    /// Whether the operation was accepted.
    pub accepted: bool,
    /// Operation id, or `null` when the source rejects without one.
    pub operation_id: Option<String>,
    /// Error payload, or `null` for a successful operation.
    pub error: Option<AgentOperationError>,
}

/// Accepted/rejected response for queued operations.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AgentQueueResponse {
    /// Whether the queue entry was accepted.
    pub accepted: bool,
    /// Queue entry id, or `null` when rejected.
    pub entry_id: Option<String>,
    /// Error payload, or `null` for an accepted entry.
    pub error: Option<AgentOperationError>,
}

/// Request to compact the current lane.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AgentCompactionRequest {
    /// Custom instructions, or `null` for the default compaction prompt.
    pub custom_instructions: Option<String>,
}

/// Request to navigate the lane tree.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AgentNavigationRequest {
    /// Target entry id, or `null` for the source default target.
    pub target_id: Option<String>,
    /// Whether to summarize while navigating.
    pub summarize: bool,
    /// Optional navigation label, represented as a required nullable field.
    pub label: Option<String>,
    /// Optional custom instructions, represented as a required nullable field.
    pub custom_instructions: Option<String>,
}

/// Outcome of cancelling one queued entry.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum CancelQueuedOutcome {
    /// The entry was cancelled before consumption.
    Cancelled,
    /// The entry had already been consumed.
    AlreadyConsumed,
    /// No matching entry was found.
    NotFound,
}

impl ProductJsonConvert for AgentPromptImage {
    fn from_json(value: JsonValue) -> Result<Self, ServiceError> {
        let fields = object(&value, "agent prompt image")?;
        let image_type = string(
            required(fields, "type", "agent prompt image.type")?,
            "agent prompt image.type",
        )?;
        if image_type != "image" {
            return Err(super::invalid("agent prompt image.type"));
        }
        let data = string(
            required(fields, "data", "agent prompt image.data")?,
            "agent prompt image.data",
        )?;
        let mime_type = string(
            required(fields, "mimeType", "agent prompt image.mimeType")?,
            "agent prompt image.mimeType",
        )?;
        Ok(Self {
            r#type: image_type,
            data,
            mime_type,
        })
    }

    fn into_json(self) -> Result<JsonValue, ServiceError> {
        Ok(json_object([
            ("type", JsonValue::String(self.r#type.into())),
            ("data", JsonValue::String(self.data.into())),
            ("mimeType", JsonValue::String(self.mime_type.into())),
        ]))
    }
}

impl ProductJsonConvert for AgentPromptRequest {
    fn from_json(value: JsonValue) -> Result<Self, ServiceError> {
        let fields = object(&value, "agent prompt request")?;
        let message = string(
            required(fields, "message", "agent prompt request.message")?,
            "agent prompt request.message",
        )?;
        let images = nullable(
            required(fields, "images", "agent prompt request.images")?,
            |value| {
                array(value, "agent prompt request.images")?
                    .iter()
                    .cloned()
                    .map(AgentPromptImage::from_json)
                    .collect()
            },
        )?;
        Ok(Self { message, images })
    }

    fn into_json(self) -> Result<JsonValue, ServiceError> {
        let images = match self.images {
            Some(images) => JsonValue::Array(
                images
                    .into_iter()
                    .map(AgentPromptImage::into_json)
                    .collect::<Result<Vec<_>, _>>()?,
            ),
            None => JsonValue::Null,
        };
        Ok(json_object([
            ("message", JsonValue::String(self.message.into())),
            ("images", images),
        ]))
    }
}

impl ProductJsonConvert for AgentOperationError {
    fn from_json(value: JsonValue) -> Result<Self, ServiceError> {
        let fields = object(&value, "agent operation error")?;
        let code = string(
            required(fields, "code", "agent operation error.code")?,
            "agent operation error.code",
        )?;
        let message = string(
            required(fields, "message", "agent operation error.message")?,
            "agent operation error.message",
        )?;
        Ok(Self { code, message })
    }

    fn into_json(self) -> Result<JsonValue, ServiceError> {
        Ok(json_object([
            ("code", JsonValue::String(self.code.into())),
            ("message", JsonValue::String(self.message.into())),
        ]))
    }
}

impl ProductJsonConvert for AgentOperationResponse {
    fn from_json(value: JsonValue) -> Result<Self, ServiceError> {
        let fields = object(&value, "agent operation response")?;
        let accepted = bool_value(
            required(fields, "accepted", "agent operation response.accepted")?,
            "agent operation response.accepted",
        )?;
        let operation_id = nullable_string(
            required(
                fields,
                "operationId",
                "agent operation response.operationId",
            )?,
            "agent operation response.operationId",
        )?;
        let error = nullable(
            required(fields, "error", "agent operation response.error")?,
            |value| AgentOperationError::from_json(value.clone()),
        )?;
        Ok(Self {
            accepted,
            operation_id,
            error,
        })
    }

    fn into_json(self) -> Result<JsonValue, ServiceError> {
        let error = match self.error {
            Some(error) => error.into_json()?,
            None => JsonValue::Null,
        };
        Ok(json_object([
            ("accepted", JsonValue::Bool(self.accepted)),
            (
                "operationId",
                self.operation_id
                    .map_or(JsonValue::Null, |id| JsonValue::String(id.into())),
            ),
            ("error", error),
        ]))
    }
}

impl ProductJsonConvert for AgentQueueResponse {
    fn from_json(value: JsonValue) -> Result<Self, ServiceError> {
        let fields = object(&value, "agent queue response")?;
        let accepted = bool_value(
            required(fields, "accepted", "agent queue response.accepted")?,
            "agent queue response.accepted",
        )?;
        let entry_id = nullable_string(
            required(fields, "entryId", "agent queue response.entryId")?,
            "agent queue response.entryId",
        )?;
        let error = nullable(
            required(fields, "error", "agent queue response.error")?,
            |value| AgentOperationError::from_json(value.clone()),
        )?;
        Ok(Self {
            accepted,
            entry_id,
            error,
        })
    }

    fn into_json(self) -> Result<JsonValue, ServiceError> {
        let error = match self.error {
            Some(error) => error.into_json()?,
            None => JsonValue::Null,
        };
        Ok(json_object([
            ("accepted", JsonValue::Bool(self.accepted)),
            (
                "entryId",
                self.entry_id
                    .map_or(JsonValue::Null, |id| JsonValue::String(id.into())),
            ),
            ("error", error),
        ]))
    }
}

impl ProductJsonConvert for AgentCompactionRequest {
    fn from_json(value: JsonValue) -> Result<Self, ServiceError> {
        let fields = object(&value, "agent compaction request")?;
        let custom_instructions = nullable_string(
            required(
                fields,
                "customInstructions",
                "agent compaction request.customInstructions",
            )?,
            "agent compaction request.customInstructions",
        )?;
        Ok(Self {
            custom_instructions,
        })
    }

    fn into_json(self) -> Result<JsonValue, ServiceError> {
        Ok(json_object([(
            "customInstructions",
            self.custom_instructions
                .map_or(JsonValue::Null, |value| JsonValue::String(value.into())),
        )]))
    }
}

impl ProductJsonConvert for AgentNavigationRequest {
    fn from_json(value: JsonValue) -> Result<Self, ServiceError> {
        let fields = object(&value, "agent navigation request")?;
        let target_id = nullable_string(
            required(fields, "targetId", "agent navigation request.targetId")?,
            "agent navigation request.targetId",
        )?;
        let summarize = bool_value(
            required(fields, "summarize", "agent navigation request.summarize")?,
            "agent navigation request.summarize",
        )?;
        let label = nullable_string(
            required(fields, "label", "agent navigation request.label")?,
            "agent navigation request.label",
        )?;
        let custom_instructions = nullable_string(
            required(
                fields,
                "customInstructions",
                "agent navigation request.customInstructions",
            )?,
            "agent navigation request.customInstructions",
        )?;
        Ok(Self {
            target_id,
            summarize,
            label,
            custom_instructions,
        })
    }

    fn into_json(self) -> Result<JsonValue, ServiceError> {
        Ok(json_object([
            (
                "targetId",
                self.target_id
                    .map_or(JsonValue::Null, |value| JsonValue::String(value.into())),
            ),
            ("summarize", JsonValue::Bool(self.summarize)),
            (
                "label",
                self.label
                    .map_or(JsonValue::Null, |value| JsonValue::String(value.into())),
            ),
            (
                "customInstructions",
                self.custom_instructions
                    .map_or(JsonValue::Null, |value| JsonValue::String(value.into())),
            ),
        ]))
    }
}

impl ProductJsonConvert for CancelQueuedOutcome {
    fn from_json(value: JsonValue) -> Result<Self, ServiceError> {
        let fields = object(&value, "cancel queued response")?;
        let outcome = string(
            required(fields, "outcome", "cancel queued response.outcome")?,
            "cancel queued response.outcome",
        )?;
        match outcome.as_str() {
            "cancelled" => Ok(Self::Cancelled),
            "already_consumed" => Ok(Self::AlreadyConsumed),
            "not_found" => Ok(Self::NotFound),
            _ => Err(super::invalid("cancel queued response.outcome")),
        }
    }

    fn into_json(self) -> Result<JsonValue, ServiceError> {
        let outcome = match self {
            Self::Cancelled => "cancelled",
            Self::AlreadyConsumed => "already_consumed",
            Self::NotFound => "not_found",
        };
        Ok(json_object([(
            "outcome",
            JsonValue::String(outcome.into()),
        )]))
    }
}
