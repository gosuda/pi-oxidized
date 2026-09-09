//! Session directory and management service contracts.
//!
//! Source mapping: `experimental/services/sessions.ts`. These declarations
//! contain no server/session provider behavior.

use crate::remote::schemas::ServerId;
use pi_agent::service::error::ServiceError;
use pi_agent::service::value::JsonValue;

use super::{
    ProductJsonConvert, array, integer, invalid, json_object, object, optional, required, string,
};

/// Chord service identifier for the server-wide session directory.
pub const SESSION_DIRECTORY_ID: &str = "pi.session-directory";
/// Chord service identifier for session lifecycle management.
pub const SESSION_MANAGEMENT_ID: &str = "pi.session-management";

/// Replicated state member of [`SESSION_DIRECTORY_ID`].
pub const SESSION_DIRECTORY_STATE_MEMBER: &str = "state";
/// Session-management method names.
pub const SESSION_MANAGEMENT_CREATE_MEMBER: &str = "create";
/// Session-management method for removing a session.
pub const SESSION_MANAGEMENT_REMOVE_MEMBER: &str = "remove";
/// Session-management method for attaching to a session.
pub const SESSION_MANAGEMENT_ATTACH_MEMBER: &str = "attach";
/// Session-management method for detaching from a session.
pub const SESSION_MANAGEMENT_DETACH_MEMBER: &str = "detach";

const MAX_SAFE_INTEGER: i64 = 9_007_199_254_740_991;
const MAX_SAFE_INTEGER_F64: f64 = 9_007_199_254_740_991.0;

/// Address of a session on one product server.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SessionAddress {
    /// Canonical server identity.
    pub server_id: ServerId,
    /// Server-local session identifier.
    pub session_id: String,
}

/// Session listing row. The address fields are flattened on the wire.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SessionSummary {
    /// Flattened server/session address.
    pub address: SessionAddress,
    /// Creation timestamp in milliseconds.
    pub created_at: i64,
}

/// Options accepted by the `create` method.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct SessionCreateOptions {
    /// Optional caller-selected session identifier. `None` omits `id`.
    pub id: Option<String>,
}

/// Replicated server-wide session directory state.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SessionDirectoryState {
    /// Source `number` revision, retained as a canonical binary64 integer.
    pub revision: pi_agent::service::value::JsInteger,
    /// Current session rows.
    pub sessions: Vec<SessionSummary>,
}

impl ProductJsonConvert for SessionAddress {
    fn from_json(value: JsonValue) -> Result<Self, ServiceError> {
        let fields = object(&value, "session address")?;
        let server_id = ServerId::new(string(
            required(fields, "serverId", "session address.serverId")?,
            "session address.serverId",
        )?)
        .map_err(|_| invalid("session address.serverId"))?;
        let session_id = string(
            required(fields, "sessionId", "session address.sessionId")?,
            "session address.sessionId",
        )?;
        Ok(Self {
            server_id,
            session_id,
        })
    }

    fn into_json(self) -> Result<JsonValue, ServiceError> {
        Ok(json_object([
            (
                "serverId",
                JsonValue::String(pi_agent::service::value::JsString::from_utf8(
                    self.server_id.as_str(),
                )),
            ),
            ("sessionId", JsonValue::String(self.session_id.into())),
        ]))
    }
}

impl ProductJsonConvert for SessionSummary {
    fn from_json(value: JsonValue) -> Result<Self, ServiceError> {
        let fields = object(&value, "session summary")?;
        let address = SessionAddress::from_json(value.clone())?;
        let created_at_value = required(fields, "createdAt", "session summary.createdAt")?;
        let created_at = created_at_value
            .as_f64()
            .filter(|number| number.is_finite() && number.fract() == 0.0)
            .ok_or_else(|| invalid("session summary.createdAt"))?;
        if !(-MAX_SAFE_INTEGER_F64..=MAX_SAFE_INTEGER_F64).contains(&created_at) {
            return Err(invalid("session summary.createdAt"));
        }
        #[expect(
            clippy::cast_possible_truncation,
            reason = "bounds and integral check make this conversion exact"
        )]
        let created_at = created_at as i64;
        Ok(Self {
            address,
            created_at,
        })
    }

    fn into_json(self) -> Result<JsonValue, ServiceError> {
        let Some(mut fields) = self.address.into_json()?.into_object() else {
            return Err(invalid("session summary address"));
        };
        if self.created_at < -MAX_SAFE_INTEGER || self.created_at > MAX_SAFE_INTEGER {
            return Err(invalid("session summary.createdAt"));
        }
        #[expect(
            clippy::cast_precision_loss,
            reason = "bounded to the exact binary64 integer range"
        )]
        let created_at = self.created_at as f64;
        fields.insert("createdAt".into(), JsonValue::Number(created_at));
        Ok(JsonValue::Object(fields))
    }
}

impl ProductJsonConvert for SessionCreateOptions {
    fn from_json(value: JsonValue) -> Result<Self, ServiceError> {
        let fields = object(&value, "session create options")?;
        let id = match optional(fields, "id") {
            None => None,
            Some(value) => Some(string(value, "session create options.id")?),
        };
        Ok(Self { id })
    }

    fn into_json(self) -> Result<JsonValue, ServiceError> {
        let mut fields = std::collections::BTreeMap::new();
        if let Some(id) = self.id {
            fields.insert("id".into(), JsonValue::String(id.into()));
        }
        Ok(JsonValue::Object(fields))
    }
}

impl ProductJsonConvert for SessionDirectoryState {
    fn from_json(value: JsonValue) -> Result<Self, ServiceError> {
        let fields = object(&value, "session directory state")?;
        let revision = integer(
            required(fields, "revision", "session directory state.revision")?,
            "session directory state.revision",
        )?;
        let sessions = array(
            required(fields, "sessions", "session directory state.sessions")?,
            "session directory state.sessions",
        )?
        .iter()
        .cloned()
        .map(SessionSummary::from_json)
        .collect::<Result<Vec<_>, _>>()?;
        Ok(Self { revision, sessions })
    }

    fn into_json(self) -> Result<JsonValue, ServiceError> {
        let sessions = self
            .sessions
            .into_iter()
            .map(SessionSummary::into_json)
            .collect::<Result<Vec<_>, _>>()?;
        Ok(json_object([
            ("revision", JsonValue::Number(self.revision.as_f64())),
            ("sessions", JsonValue::Array(sessions)),
        ]))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn session_summary(created_at: f64) -> JsonValue {
        json_object([
            (
                "serverId",
                JsonValue::String("00000000-0000-4000-8000-000000000000".into()),
            ),
            ("sessionId", JsonValue::String("session".into())),
            ("createdAt", JsonValue::Number(created_at)),
        ])
    }

    #[test]
    fn decode_accepts_maximum_safe_integer_timestamp() -> Result<(), ServiceError> {
        let summary = SessionSummary::from_json(session_summary(MAX_SAFE_INTEGER_F64))?;
        assert_eq!(summary.created_at, MAX_SAFE_INTEGER);
        Ok(())
    }

    #[test]
    fn decode_rejects_timestamp_past_safe_integer_range() -> Result<(), ServiceError> {
        let Err(error) = SessionSummary::from_json(session_summary(MAX_SAFE_INTEGER_F64 + 1.0))
        else {
            return Err(ServiceError::local(
                "timestamp past safe integer range was accepted",
            ));
        };
        assert_eq!(
            error.to_string(),
            "Invalid product service session summary.createdAt"
        );
        Ok(())
    }
}
