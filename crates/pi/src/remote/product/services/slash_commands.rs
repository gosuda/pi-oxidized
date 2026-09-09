//! Local slash-command service contracts.
//!
//! Source mapping: `experimental/services/slash-commands.ts`. The service is
//! process-local and is never published through the remote provider. Command
//! callbacks are facet-host RPCs; the native service payload carries only the
//! data advertised to the client/TUI.

use pi_agent::service::error::ServiceError;
use pi_agent::service::value::JsonValue;

use super::agent_controller::{AgentOperationResponse, AgentQueueResponse};
use super::{ProductJsonConvert, object, optional, required, string};
/// Chord service identifier for the process-local slash-command service.
pub const SLASH_COMMANDS_ID: &str = "pi.local.slash-commands";

/// Wire method that asks the local slash-command service to register a contribution.
pub const SLASH_COMMANDS_REGISTER_MEMBER: &str = "register";
/// Wire method that asks the local slash-command service to stage a same-name replacement.
pub const SLASH_COMMANDS_REPLACE_MEMBER: &str = "replace";
/// Wire method that asks the local slash-command service for registered contributions.
pub const SLASH_COMMANDS_LIST_MEMBER: &str = "list";
/// Wire method that asks the local slash-command service to subscribe to contribution updates.
pub const SLASH_COMMANDS_SUBSCRIBE_MEMBER: &str = "subscribe";

/// Completion row shown by a slash-command provider.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SlashCommandCompletion {
    /// Completion value inserted into the input.
    pub value: String,
    /// Display label.
    pub label: String,
    /// Optional explanatory text. `None` omits the source field.
    pub description: Option<String>,
}

/// Data portion of a slash-command contribution.
///
/// `run` and `getArgumentCompletions` are intentionally not native fields:
/// source callbacks become explicit facet-host bridge calls, while this value
/// is the stable metadata exchanged with the TUI.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SlashCommandContribution {
    /// Command name without the leading slash.
    pub name: String,
    /// Optional command description.
    pub description: Option<String>,
    /// Optional argument hint.
    pub argument_hint: Option<String>,
}

/// Bridge-facing alias for the metadata payload carried out of the local
/// service.
pub type SlashCommandCompletionWire = SlashCommandCompletion;
/// Bridge-facing alias for the metadata payload carried out of the local
/// service.
pub type SlashCommandContributionWire = SlashCommandContribution;

/// Result a local command callback may return. `None` represents source
/// `undefined` at the callback boundary.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SlashCommandRunResult {
    /// An operation was accepted or rejected by the agent controller.
    Operation(AgentOperationResponse),
    /// A queue entry was accepted or rejected by the agent controller.
    Queue(AgentQueueResponse),
}

impl ProductJsonConvert for SlashCommandCompletion {
    fn from_json(value: JsonValue) -> Result<Self, ServiceError> {
        let fields = object(&value, "slash command completion")?;
        let value = string(
            required(fields, "value", "slash command completion.value")?,
            "slash command completion.value",
        )?;
        let label = string(
            required(fields, "label", "slash command completion.label")?,
            "slash command completion.label",
        )?;
        let description = match optional(fields, "description") {
            None => None,
            Some(value) if value.is_null() => {
                return Err(super::invalid("slash command completion.description"));
            }
            Some(value) => Some(string(value, "slash command completion.description")?),
        };
        Ok(Self {
            value,
            label,
            description,
        })
    }

    fn into_json(self) -> Result<JsonValue, ServiceError> {
        let mut fields = std::collections::BTreeMap::new();
        fields.insert("value".into(), JsonValue::String(self.value.into()));
        fields.insert("label".into(), JsonValue::String(self.label.into()));
        if let Some(description) = self.description {
            fields.insert("description".into(), JsonValue::String(description.into()));
        }
        Ok(JsonValue::Object(fields))
    }
}

impl ProductJsonConvert for SlashCommandContribution {
    fn from_json(value: JsonValue) -> Result<Self, ServiceError> {
        let fields = object(&value, "slash command contribution")?;
        let name = string(
            required(fields, "name", "slash command contribution.name")?,
            "slash command contribution.name",
        )?;
        let description = match optional(fields, "description") {
            None => None,
            Some(value) if value.is_null() => {
                return Err(super::invalid("slash command contribution.description"));
            }
            Some(value) => Some(string(value, "slash command contribution.description")?),
        };
        let argument_hint = match optional(fields, "argumentHint") {
            None => None,
            Some(value) if value.is_null() => {
                return Err(super::invalid("slash command contribution.argumentHint"));
            }
            Some(value) => Some(string(value, "slash command contribution.argumentHint")?),
        };
        Ok(Self {
            name,
            description,
            argument_hint,
        })
    }

    fn into_json(self) -> Result<JsonValue, ServiceError> {
        let mut fields = std::collections::BTreeMap::new();
        fields.insert("name".into(), JsonValue::String(self.name.into()));
        if let Some(description) = self.description {
            fields.insert("description".into(), JsonValue::String(description.into()));
        }
        if let Some(argument_hint) = self.argument_hint {
            fields.insert(
                "argumentHint".into(),
                JsonValue::String(argument_hint.into()),
            );
        }
        Ok(JsonValue::Object(fields))
    }
}

impl ProductJsonConvert for SlashCommandRunResult {
    fn from_json(value: JsonValue) -> Result<Self, ServiceError> {
        let fields = object(&value, "slash command run result")?;
        if optional(fields, "operationId").is_some() {
            return AgentOperationResponse::from_json(value).map(Self::Operation);
        }
        if optional(fields, "entryId").is_some() {
            return AgentQueueResponse::from_json(value).map(Self::Queue);
        }
        Err(super::invalid("slash command run result"))
    }

    fn into_json(self) -> Result<JsonValue, ServiceError> {
        match self {
            Self::Operation(response) => response.into_json(),
            Self::Queue(response) => response.into_json(),
        }
    }
}
