//! Main-lane transcript state service contract.
//!
//! Source mapping: `experimental/services/transcript.ts`. Watching,
//! reduction, and rebase scheduling remain provider-owned. The event field is
//! intentionally opaque canonical JSON because the native harness event
//! filter is a provider concern.

use pi_agent::harness::snapshot::LaneSnapshot;
use pi_agent::service::error::ServiceError;
use pi_agent::service::value::JsonValue;

use super::{invalid, json_object, nullable, object, required, serde_decode, serde_encode, ProductJsonConvert};

/// Chord service identifier for the attached lane transcript.
pub const TRANSCRIPT_ID: &str = "pi.transcript";
/// Replicated state member name.
pub const TRANSCRIPT_STATE_MEMBER: &str = "state";

/// Coherent transcript state replicated through Chord.
#[derive(Clone, Debug, PartialEq)]
pub struct TranscriptState {
    /// Current lane snapshot, or `null` before activation.
    pub snapshot: Option<LaneSnapshot>,
    /// The filtered source watch event retained for presentation effects.
    pub event: Option<JsonValue>,
}

impl ProductJsonConvert for TranscriptState {
    fn from_json(value: JsonValue) -> Result<Self, ServiceError> {
        let fields = object(&value, "transcript state")?;
        let snapshot = nullable(required(fields, "snapshot", "transcript state.snapshot")?, |value| {
            serde_decode(value, "transcript state.snapshot")
        })?;
        let event = nullable(required(fields, "event", "transcript state.event")?, |value| {
            if pi_agent::service::value::is_json_value(value) {
                Ok(value.clone())
            } else {
                Err(invalid("transcript state.event"))
            }
        })?;
        Ok(Self { snapshot, event })
    }

    fn into_json(self) -> Result<JsonValue, ServiceError> {
        let snapshot = match self.snapshot {
            Some(snapshot) => serde_encode(&snapshot, "transcript state.snapshot")?,
            None => JsonValue::Null,
        };
        let event = self.event.unwrap_or(JsonValue::Null);
        Ok(json_object([("snapshot", snapshot), ("event", event)]))
    }
}
