//! Agent transcript messages, including opaque custom app messages.

use pi_ai::{ImageContent, Message, TextContent, UserContent, UserMessage, UserMessageContent};
use serde::de::Error as _;
use serde::ser::SerializeMap;
use serde::{Deserialize, Deserializer, Serialize, Serializer};
use serde_json::{Map, Value};

/// Opaque custom message preserved for app-specific transcript roles.
///
/// Unknown roles such as `bashExecution`, `branchSummary`, and
/// `compactionSummary` round-trip through the flatten payload without
/// interpretation by `pi-agent`. The reserved `role` key is never stored in
/// [`CustomAgentMessage::payload`].
#[derive(Clone, Debug, PartialEq)]
pub struct CustomAgentMessage {
    /// Discriminating role string for the custom message.
    pub role: String,
    /// Remaining JSON fields preserved across serialization.
    pub payload: Map<String, Value>,
}

impl CustomAgentMessage {
    /// Creates a custom message, stripping any reserved `role` key from payload.
    #[must_use]
    pub fn new(role: impl Into<String>, mut payload: Map<String, Value>) -> Self {
        payload.remove("role");
        Self {
            role: role.into(),
            payload,
        }
    }
}

impl Serialize for CustomAgentMessage {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        let mut map = serializer.serialize_map(Some(self.payload.len().saturating_add(1)))?;
        map.serialize_entry("role", &self.role)?;
        for (key, value) in &self.payload {
            if key != "role" {
                map.serialize_entry(key, value)?;
            }
        }
        map.end()
    }
}

impl<'de> Deserialize<'de> for CustomAgentMessage {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let mut value = Map::<String, Value>::deserialize(deserializer)?;
        let role = match value.remove("role") {
            Some(Value::String(role)) => role,
            Some(_) => {
                return Err(D::Error::custom(
                    "custom agent message role must be a string",
                ));
            }
            None => return Err(D::Error::custom("custom agent message missing role")),
        };
        Ok(Self {
            role,
            payload: value,
        })
    }
}

/// Transcript message accepted by the agent loop.
///
/// LLM-compatible messages reuse [`pi_ai::Message`]. Unknown roles fall through
/// to [`CustomAgentMessage`] and remain opaque until product code maps them.
#[derive(Clone, Debug, PartialEq)]
pub enum AgentMessage {
    /// Standard provider-facing message.
    Llm(Box<Message>),
    /// App-specific custom message.
    Custom(CustomAgentMessage),
}

impl Serialize for AgentMessage {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        match self {
            Self::Llm(message) => message.serialize(serializer),
            Self::Custom(message) => message.serialize(serializer),
        }
    }
}

impl<'de> Deserialize<'de> for AgentMessage {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let value = Value::deserialize(deserializer)?;
        let role = value
            .get("role")
            .and_then(Value::as_str)
            .ok_or_else(|| D::Error::custom("agent message missing role"))?;

        match role {
            "user" | "assistant" | "toolResult" => {
                let message = Message::deserialize(value).map_err(D::Error::custom)?;
                Ok(Self::Llm(Box::new(message)))
            }
            _ => {
                let message = CustomAgentMessage::deserialize(value).map_err(D::Error::custom)?;
                Ok(Self::Custom(message))
            }
        }
    }
}

impl AgentMessage {
    /// Returns the message role string.
    #[must_use]
    pub fn role(&self) -> &str {
        match self {
            Self::Llm(message) => match message.as_ref() {
                Message::User(_) => "user",
                Message::Assistant(_) => "assistant",
                Message::ToolResult(_) => "toolResult",
            },
            Self::Custom(message) => message.role.as_str(),
        }
    }

    /// Returns true when this message is an LLM-compatible variant.
    #[must_use]
    pub const fn is_llm(&self) -> bool {
        matches!(self, Self::Llm(_))
    }

    /// Borrows the inner LLM message when present.
    #[must_use]
    pub fn as_llm(&self) -> Option<&Message> {
        match self {
            Self::Llm(message) => Some(message.as_ref()),
            Self::Custom(_) => None,
        }
    }

    /// Consumes the value and returns the inner LLM message when present.
    #[must_use]
    pub fn into_llm(self) -> Option<Message> {
        match self {
            Self::Llm(message) => Some(*message),
            Self::Custom(_) => None,
        }
    }
}

/// Default conversion used when product code does not supply a custom mapper.
///
/// Keeps user, assistant, and tool-result messages and drops custom roles.
#[must_use]
pub fn default_convert_to_llm(messages: &[AgentMessage]) -> Vec<Message> {
    messages
        .iter()
        .filter_map(AgentMessage::as_llm)
        .cloned()
        .collect()
}

/// Builds a user [`AgentMessage`] with optional image attachments.
#[must_use]
pub fn user_text(
    text: impl Into<String>,
    images: impl IntoIterator<Item = ImageContent>,
) -> AgentMessage {
    let text = text.into();
    let images: Vec<ImageContent> = images.into_iter().collect();
    let content = if images.is_empty() {
        UserMessageContent::Text(text)
    } else {
        let mut blocks = Vec::with_capacity(images.len().saturating_add(1));
        blocks.push(UserContent::Text(TextContent::new(text)));
        blocks.extend(images.into_iter().map(UserContent::Image));
        UserMessageContent::Blocks(blocks)
    };
    AgentMessage::Llm(Box::new(Message::User(UserMessage::new(
        content,
        now_millis(),
    ))))
}

/// Current Unix timestamp in milliseconds.
#[must_use]
pub fn now_millis() -> i64 {
    use std::time::{SystemTime, UNIX_EPOCH};

    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .ok()
        .and_then(|duration| i64::try_from(duration.as_millis()).ok())
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn custom_message_preserves_unknown_payload_fields() -> Result<(), serde_json::Error> {
        let raw = json!({
            "role": "bashExecution",
            "command": "ls -la",
            "exitCode": 0,
            "nested": { "ok": true }
        });

        let message: AgentMessage = serde_json::from_value(raw.clone())?;
        let AgentMessage::Custom(custom) = &message else {
            return Err(serde::de::Error::custom("expected custom message"));
        };
        assert_eq!(custom.role, "bashExecution");
        assert!(!custom.payload.contains_key("role"));
        assert_eq!(custom.payload.get("command"), Some(&json!("ls -la")));
        assert_eq!(custom.payload.get("exitCode"), Some(&json!(0)));
        assert_eq!(custom.payload.get("nested"), Some(&json!({ "ok": true })));

        let encoded = serde_json::to_value(&message)?;
        assert_eq!(encoded, raw);
        Ok(())
    }

    #[test]
    fn custom_message_constructor_strips_reserved_role_from_payload()
    -> Result<(), serde_json::Error> {
        let custom = CustomAgentMessage::new(
            "notification",
            Map::from_iter([
                ("role".to_owned(), json!("spoofed")),
                ("text".to_owned(), json!("n")),
            ]),
        );
        assert_eq!(custom.role, "notification");
        assert!(!custom.payload.contains_key("role"));
        let encoded = serde_json::to_value(&custom)?;
        assert_eq!(encoded["role"], json!("notification"));
        assert_eq!(encoded["text"], json!("n"));
        Ok(())
    }

    #[test]
    fn default_convert_to_llm_keeps_llm_and_drops_custom() {
        let user = AgentMessage::Llm(Box::new(Message::User(UserMessage::new(
            UserMessageContent::Text("hi".to_owned()),
            1,
        ))));
        let custom = AgentMessage::Custom(CustomAgentMessage::new(
            "notification",
            Map::from_iter([("text".to_owned(), json!("n"))]),
        ));

        let converted = default_convert_to_llm(&[user.clone(), custom]);
        assert_eq!(converted.len(), 1);
        assert!(matches!(converted[0], Message::User(_)));
        assert_eq!(user.role(), "user");
        assert!(user.is_llm());
        assert!(user.as_llm().is_some());
    }

    #[test]
    fn llm_user_message_round_trips_as_llm_variant() -> Result<(), serde_json::Error> {
        let raw = json!({
            "role": "user",
            "content": "hello",
            "timestamp": 42
        });
        let message: AgentMessage = serde_json::from_value(raw.clone())?;
        assert!(message.is_llm());
        assert_eq!(message.role(), "user");
        let encoded = serde_json::to_value(&message)?;
        assert_eq!(encoded, raw);
        Ok(())
    }

    #[test]
    fn malformed_known_role_does_not_become_custom() {
        let raw = json!({
            "role": "user",
            "content": 123
        });
        let result = serde_json::from_value::<AgentMessage>(raw);
        assert!(result.is_err());
    }
}
