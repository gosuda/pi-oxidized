//! Source-C service contracts for the development-only product host.
//!
//! The declarations mirror `packages/coding-agent/src/experimental/services`.
//! They describe IDs, member names, and payload shapes only. Provider
//! implementations and service-registry mutation belong to the product
//! provider owners.

use std::collections::BTreeMap;

use pi_agent::service::error::ServiceError;
use pi_agent::service::value::{JsInteger, JsObject, JsString, JsonValue};
use serde::Serialize;
use serde::de::DeserializeOwned;
pub mod agent_controller;
pub mod models;
pub mod plugins;
pub mod presentation_ui;
pub mod sessions;
pub mod slash_commands;
pub mod transcript;

/// Conversion boundary used by product providers and clients.
///
/// Product service arguments and results are canonical Chord [`JsonValue`]
/// trees. Implementations preserve source field names and nullability, and
/// return a [`ServiceError::Local`] when a typed adapter cannot consume the
/// tree. Serialization is fallible so native serde types cannot silently turn
/// an invalid value into a placeholder.
pub trait ProductJsonConvert: Sized {
    /// Decodes one canonical value into the typed product payload.
    ///
    /// # Errors
    ///
    /// Returns [`ServiceError::Local`] when `value` does not match the
    /// source-shaped payload expected by the implementation.
    fn from_json(value: JsonValue) -> Result<Self, ServiceError>;

    /// Encodes the typed product payload as a canonical value.
    ///
    /// # Errors
    ///
    /// Returns [`ServiceError::Local`] when a nested value cannot be represented
    /// in the canonical value tree.
    fn into_json(self) -> Result<JsonValue, ServiceError>;
}

pub(crate) fn invalid(description: impl Into<String>) -> ServiceError {
    let description: String = description.into();
    ServiceError::local(format!("Invalid product service {description}"))
}

pub(crate) fn object<'a>(
    value: &'a JsonValue,
    description: &str,
) -> Result<&'a JsObject, ServiceError> {
    value.as_object().ok_or_else(|| invalid(description))
}

pub(crate) fn array<'a>(
    value: &'a JsonValue,
    description: &str,
) -> Result<&'a [JsonValue], ServiceError> {
    value
        .as_array()
        .map(Vec::as_slice)
        .ok_or_else(|| invalid(description))
}

pub(crate) fn required<'a>(
    fields: &'a JsObject,
    name: &str,
    description: &str,
) -> Result<&'a JsonValue, ServiceError> {
    let key = JsString::from_utf8(name);
    fields.get(&key).ok_or_else(|| invalid(description))
}

pub(crate) fn optional<'a>(fields: &'a JsObject, name: &str) -> Option<&'a JsonValue> {
    let key = JsString::from_utf8(name);
    fields.get(&key)
}

pub(crate) fn string(value: &JsonValue, description: &str) -> Result<String, ServiceError> {
    let value = value.as_str().ok_or_else(|| invalid(description))?;
    value
        .try_to_utf8()
        .map_err(|error| invalid(format!("{description}: {error}")))
}

pub(crate) fn nullable_string(
    value: &JsonValue,
    description: &str,
) -> Result<Option<String>, ServiceError> {
    if value.is_null() {
        Ok(None)
    } else {
        string(value, description).map(Some)
    }
}

pub(crate) fn bool_value(value: &JsonValue, description: &str) -> Result<bool, ServiceError> {
    value.as_bool().ok_or_else(|| invalid(description))
}

pub(crate) fn integer(value: &JsonValue, description: &str) -> Result<JsInteger, ServiceError> {
    let number = value.as_f64().ok_or_else(|| invalid(description))?;
    JsInteger::new(number).map_err(|error| invalid(format!("{description}: {error}")))
}

pub(crate) fn nullable<T>(
    value: &JsonValue,
    decode: impl FnOnce(&JsonValue) -> Result<T, ServiceError>,
) -> Result<Option<T>, ServiceError> {
    if value.is_null() {
        Ok(None)
    } else {
        decode(value).map(Some)
    }
}

pub(crate) fn strings(
    values: &[JsonValue],
    description: &str,
) -> Result<Vec<String>, ServiceError> {
    values
        .iter()
        .map(|value| string(value, description))
        .collect()
}

pub(crate) fn json_object(
    fields: impl IntoIterator<Item = (&'static str, JsonValue)>,
) -> JsonValue {
    let mut object = BTreeMap::new();
    for (name, value) in fields {
        object.insert(JsString::from_utf8(name), value);
    }
    JsonValue::Object(object)
}

pub(crate) fn serde_decode<T: DeserializeOwned>(
    value: &JsonValue,
    description: &str,
) -> Result<T, ServiceError> {
    let value = pi_agent::service::value::try_into_serde_json(value.clone())
        .map_err(|error| invalid(format!("{description}: {error}")))?;
    serde_json::from_value(value).map_err(|error| invalid(format!("{description}: {error}")))
}

pub(crate) fn serde_encode<T: Serialize>(
    value: &T,
    description: &str,
) -> Result<JsonValue, ServiceError> {
    let value =
        serde_json::to_value(value).map_err(|error| invalid(format!("{description}: {error}")))?;
    Ok(pi_agent::service::value::from_serde_json(value))
}
