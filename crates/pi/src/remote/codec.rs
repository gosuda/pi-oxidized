//! Strict RFC 8949 CBOR codec for the native remote protocol v8.
//!
//! The codec validates the complete envelope before encoding and after
//! decoding. It keeps the framing layer independent and latches incremental
//! message decoders after any malformed input.

use serde::{Deserialize, Serialize};
use thiserror::Error;

use pi_agent::service::value::JsonValue;

use crate::remote::framing::{
    DEFAULT_MAX_FRAME_LENGTH, FRAME_HEADER_LENGTH, FrameDecoder, FrameDecoderOptions, FrameError,
    assert_complete_frame, encode_frame,
};
use crate::remote::schemas::{
    ClientMessage, PROTOCOL_VERSION, ProtocolError, RpcTarget, ServerMessage, ServerId,
    SessionTarget, ServerTarget, is_server_id,
};
use crate::remote::serde_cbor::{CborValue, CborValueDeserializer, CborValueSerializer, SerError};

const DEFAULT_MAX_CBOR_CONTAINER_LENGTH: usize = 1_000_000;
const DEFAULT_MAX_CBOR_DEPTH: usize = 64;
const MAX_JSON_DEPTH: usize = 512;
const MAX_SAFE_CBOR_INTEGER: u64 = 9_007_199_254_740_991;
const MAX_SAFE_CBOR_INTEGER_F64: f64 = 9_007_199_254_740_991.0;
const MAX_PROTOCOL_ERROR_CHARS: usize = 500;

/// Configuration limits used by the CBOR payload encoder and decoder.
#[derive(Debug, Clone, Copy)]
#[expect(
    clippy::struct_field_names,
    reason = "all fields are explicit maximum limits"
)]
struct CborOptions {
    /// Maximum encoded payload bytes.
    max_byte_length: usize,
    /// Maximum array elements or map entries.
    max_container_length: usize,
    /// Maximum nested item depth.
    max_depth: usize,
}

impl CborOptions {
    fn from_frame(options: Option<FrameDecoderOptions>) -> Self {
        Self {
            max_byte_length: options.map_or(DEFAULT_MAX_FRAME_LENGTH, |value| value.max_frame_length),
            max_container_length: DEFAULT_MAX_CBOR_CONTAINER_LENGTH,
            max_depth: DEFAULT_MAX_CBOR_DEPTH,
        }
    }
}

/// Byte-level CBOR encoding or decoding error.
#[derive(Debug, Clone, PartialEq, Error)]
pub enum CborError {
    /// CBOR byte length exceeds the configured limit.
    #[error("CBOR byte length exceeds configured limit of {limit}")]
    ByteLengthExceeded {
        /// Configured maximum byte length.
        limit: usize,
    },
    /// CBOR text string length exceeds the configured limit.
    #[error("CBOR text string length exceeds configured limit of {limit}")]
    TextLengthExceeded {
        /// Configured maximum text length.
        limit: usize,
    },
    /// CBOR byte string length exceeds the configured limit.
    #[error("CBOR byte string length exceeds configured limit of {limit}")]
    ByteStringLengthExceeded {
        /// Configured maximum byte-string length.
        limit: usize,
    },
    /// CBOR array length exceeds the configured limit.
    #[error("CBOR array length exceeds configured limit of {limit}")]
    ArrayLengthExceeded {
        /// Configured maximum array length.
        limit: usize,
    },
    /// CBOR map length exceeds the configured limit.
    #[error("CBOR map length exceeds configured limit of {limit}")]
    MapLengthExceeded {
        /// Configured maximum map length.
        limit: usize,
    },
    /// CBOR nesting depth exceeds the configured limit.
    #[error("CBOR nesting depth exceeds configured limit of {limit}")]
    DepthExceeded {
        /// Configured maximum nesting depth.
        limit: usize,
    },
    /// CBOR numbers must be finite.
    #[error("CBOR numbers must be finite")]
    NonFinite,
    /// CBOR payload was truncated mid-item.
    #[error("Truncated CBOR payload")]
    Truncated,
    /// CBOR payload has trailing bytes after its top-level item.
    #[error("CBOR payload contains trailing data")]
    TrailingData,
    /// CBOR tags are outside the protocol subset.
    #[error("CBOR tags are not supported")]
    TagsNotSupported,
    /// CBOR break marker is outside the protocol subset.
    #[error("CBOR break marker is not supported")]
    BreakNotSupported,
    /// Indefinite-length CBOR items are outside the protocol subset.
    #[error("Indefinite-length CBOR {0}s are not supported")]
    IndefiniteLength(&'static str),
    /// Unsupported CBOR simple value or floating-point width.
    #[error("Unsupported CBOR simple value or floating-point width")]
    UnsupportedSimple,
    /// Malformed CBOR major type bits.
    #[error("Malformed CBOR major type")]
    MalformedMajorType,
    /// Malformed CBOR additional information field.
    #[error("Malformed CBOR additional information")]
    MalformedAdditionalInfo,
    /// CBOR integer is outside the exact integer range accepted by the wire.
    #[error("CBOR integers must be safe JavaScript integers")]
    UnsafeInteger,
    /// Decoded CBOR integer is outside the safe range.
    #[error("Decoded CBOR integer is outside the safe range")]
    DecodedUnsafeInteger,
    /// Decoded CBOR floating-point value is not finite.
    #[error("Decoded CBOR number must be finite")]
    DecodedNonFinite,
    /// CBOR map contains a duplicate key.
    #[error("CBOR map contains a duplicate key")]
    DuplicateKey,
    /// CBOR map key is not a text string.
    #[error("CBOR map keys must be strings")]
    NonStringKey,
    /// CBOR text string contains invalid UTF-8.
    #[error("CBOR text string contains invalid UTF-8")]
    InvalidUtf8,
}

struct CborEncoder<'a> {
    buf: Vec<u8>,
    options: &'a CborOptions,
}

impl<'a> CborEncoder<'a> {
    fn new(options: &'a CborOptions) -> Self {
        Self {
            buf: Vec::with_capacity(256.min(options.max_byte_length)),
            options,
        }
    }

    fn finish(self) -> Vec<u8> {
        self.buf
    }

    fn write_byte(&mut self, value: u8) -> Result<(), CborError> {
        self.ensure_capacity(1)?;
        self.buf.push(value);
        Ok(())
    }

    fn write_bytes(&mut self, value: &[u8]) -> Result<(), CborError> {
        self.ensure_capacity(value.len())?;
        self.buf.extend_from_slice(value);
        Ok(())
    }

    fn ensure_capacity(&mut self, additional: usize) -> Result<(), CborError> {
        let required = self
            .buf
            .len()
            .checked_add(additional)
            .ok_or(CborError::ByteLengthExceeded {
                limit: self.options.max_byte_length,
            })?;
        if required > self.options.max_byte_length {
            return Err(CborError::ByteLengthExceeded {
                limit: self.options.max_byte_length,
            });
        }
        Ok(())
    }

    fn write_argument(&mut self, major_type: u8, value: u64) -> Result<(), CborError> {
        let prefix = major_type << 5;
        if value < 24 {
            self.write_byte(prefix | u8::try_from(value).map_err(|_| CborError::UnsafeInteger)?)
        } else if value <= u64::from(u8::MAX) {
            self.write_byte(prefix | 0x18)?;
            self.write_byte(u8::try_from(value).map_err(|_| CborError::UnsafeInteger)?)
        } else if value <= u64::from(u16::MAX) {
            self.write_byte(prefix | 0x19)?;
            self.write_bytes(
                &u16::try_from(value)
                    .map_err(|_| CborError::UnsafeInteger)?
                    .to_be_bytes(),
            )
        } else if value <= u64::from(u32::MAX) {
            self.write_byte(prefix | 0x1a)?;
            self.write_bytes(
                &u32::try_from(value)
                    .map_err(|_| CborError::UnsafeInteger)?
                    .to_be_bytes(),
            )
        } else {
            self.write_byte(prefix | 0x1b)?;
            self.write_bytes(&value.to_be_bytes())
        }
    }

    fn encode_text(&mut self, value: &str) -> Result<(), CborError> {
        let bytes = value.as_bytes();
        if bytes.len() > self.options.max_byte_length {
            return Err(CborError::TextLengthExceeded {
                limit: self.options.max_byte_length,
            });
        }
        self.write_argument(
            3,
            u64::try_from(bytes.len()).map_err(|_| CborError::TextLengthExceeded {
                limit: self.options.max_byte_length,
            })?,
        )?;
        self.write_bytes(bytes)
    }

    fn encode_value(&mut self, value: &CborValue, depth: usize) -> Result<(), CborError> {
        if depth > self.options.max_depth {
            return Err(CborError::DepthExceeded {
                limit: self.options.max_depth,
            });
        }
        match value {
            CborValue::Null => self.write_byte(0xf6),
            CborValue::Bool(true) => self.write_byte(0xf5),
            CborValue::Bool(false) => self.write_byte(0xf4),
            CborValue::UInt(number) => {
                if *number > MAX_SAFE_CBOR_INTEGER {
                    return Err(CborError::UnsafeInteger);
                }
                self.write_argument(0, *number)
            }
            CborValue::NInt(number) => {
                if *number < -(MAX_SAFE_CBOR_INTEGER as i64) {
                    return Err(CborError::UnsafeInteger);
                }
                let argument = u64::try_from(-1i128 - i128::from(*number))
                    .map_err(|_| CborError::UnsafeInteger)?;
                self.write_argument(1, argument)
            }
            CborValue::Float(number) => {
                if !number.is_finite() {
                    return Err(CborError::NonFinite);
                }
                if number.fract() == 0.0 && number.abs() > MAX_SAFE_CBOR_INTEGER_F64 {
                    return Err(CborError::UnsafeInteger);
                }
                self.write_byte(0xfb)?;
                self.write_bytes(&number.to_be_bytes())
            }
            CborValue::Text(value) => self.encode_text(value),
            CborValue::Bytes(value) => {
                if value.len() > self.options.max_byte_length {
                    return Err(CborError::ByteStringLengthExceeded {
                        limit: self.options.max_byte_length,
                    });
                }
                self.write_argument(
                    2,
                    u64::try_from(value.len()).map_err(|_| {
                        CborError::ByteStringLengthExceeded {
                            limit: self.options.max_byte_length,
                        }
                    })?,
                )?;
                self.write_bytes(value)
            }
            CborValue::Array(values) => {
                if values.len() > self.options.max_container_length {
                    return Err(CborError::ArrayLengthExceeded {
                        limit: self.options.max_container_length,
                    });
                }
                self.write_argument(
                    4,
                    u64::try_from(values.len()).map_err(|_| CborError::ArrayLengthExceeded {
                        limit: self.options.max_container_length,
                    })?,
                )?;
                for value in values {
                    self.encode_value(value, depth + 1)?;
                }
                Ok(())
            }
            CborValue::Map(entries) => {
                if entries.len() > self.options.max_container_length {
                    return Err(CborError::MapLengthExceeded {
                        limit: self.options.max_container_length,
                    });
                }
                self.write_argument(
                    5,
                    u64::try_from(entries.len()).map_err(|_| CborError::MapLengthExceeded {
                        limit: self.options.max_container_length,
                    })?,
                )?;
                for (key, value) in entries {
                    self.encode_text(key)?;
                    self.encode_value(value, depth + 1)?;
                }
                Ok(())
            }
        }
    }
}

fn encode_cbor_value(value: &CborValue, options: &CborOptions) -> Result<Vec<u8>, CborError> {
    let mut encoder = CborEncoder::new(options);
    encoder.encode_value(value, 0)?;
    Ok(encoder.finish())
}

struct CborDecoder<'a> {
    bytes: &'a [u8],
    offset: usize,
    options: &'a CborOptions,
}

impl<'a> CborDecoder<'a> {
    fn new(bytes: &'a [u8], options: &'a CborOptions) -> Self {
        Self {
            bytes,
            offset: 0,
            options,
        }
    }

    fn read_byte(&mut self) -> Result<u8, CborError> {
        let value = self
            .bytes
            .get(self.offset)
            .copied()
            .ok_or(CborError::Truncated)?;
        self.offset = self
            .offset
            .checked_add(1)
            .ok_or(CborError::Truncated)?;
        Ok(value)
    }

    fn read_bytes(&mut self, length: usize) -> Result<&'a [u8], CborError> {
        let end = self
            .offset
            .checked_add(length)
            .ok_or(CborError::Truncated)?;
        let value = self
            .bytes
            .get(self.offset..end)
            .ok_or(CborError::Truncated)?;
        self.offset = end;
        Ok(value)
    }

    fn read_argument(&mut self, additional_information: u8) -> Result<u64, CborError> {
        let value = match additional_information {
            0..=23 => u64::from(additional_information),
            24 => u64::from(self.read_byte()?),
            25 => {
                let bytes = self.read_bytes(2)?;
                let first = *bytes.first().ok_or(CborError::Truncated)?;
                let second = *bytes.get(1).ok_or(CborError::Truncated)?;
                u64::from(u16::from_be_bytes([first, second]))
            }
            26 => {
                let bytes = self.read_bytes(4)?;
                let first = *bytes.first().ok_or(CborError::Truncated)?;
                let second = *bytes.get(1).ok_or(CborError::Truncated)?;
                let third = *bytes.get(2).ok_or(CborError::Truncated)?;
                let fourth = *bytes.get(3).ok_or(CborError::Truncated)?;
                u64::from(u32::from_be_bytes([first, second, third, fourth]))
            }
            27 => {
                let bytes = self.read_bytes(8)?;
                let mut octets = [0_u8; 8];
                octets.copy_from_slice(bytes);
                u64::from_be_bytes(octets)
            }
            31 => return Err(CborError::IndefiniteLength("item")),
            _ => return Err(CborError::MalformedAdditionalInfo),
        };
        if value > MAX_SAFE_CBOR_INTEGER {
            return Err(CborError::DecodedUnsafeInteger);
        }
        Ok(value)
    }

    fn read_length(
        &mut self,
        additional_information: u8,
        kind: &'static str,
        limit: usize,
    ) -> Result<usize, CborError> {
        if additional_information == 31 {
            return Err(CborError::IndefiniteLength(kind));
        }
        let length = self.read_argument(additional_information)?;
        if length > u64::try_from(limit).unwrap_or(u64::MAX) {
            return Err(match kind {
                "byte string" => CborError::ByteStringLengthExceeded { limit },
                "text string" => CborError::TextLengthExceeded { limit },
                "array" => CborError::ArrayLengthExceeded { limit },
                _ => CborError::MapLengthExceeded { limit },
            });
        }
        usize::try_from(length).map_err(|_| match kind {
            "byte string" => CborError::ByteStringLengthExceeded { limit },
            "text string" => CborError::TextLengthExceeded { limit },
            "array" => CborError::ArrayLengthExceeded { limit },
            _ => CborError::MapLengthExceeded { limit },
        })
    }

    fn read_simple(&mut self, additional_information: u8) -> Result<CborValue, CborError> {
        match additional_information {
            20 => Ok(CborValue::Bool(false)),
            21 => Ok(CborValue::Bool(true)),
            22 => Ok(CborValue::Null),
            27 => {
                let bytes = self.read_bytes(8)?;
                let mut octets = [0_u8; 8];
                octets.copy_from_slice(bytes);
                let value = f64::from_be_bytes(octets);
                if !value.is_finite() {
                    return Err(CborError::DecodedNonFinite);
                }
                if value.fract() == 0.0 && value.abs() > MAX_SAFE_CBOR_INTEGER_F64 {
                    return Err(CborError::DecodedUnsafeInteger);
                }
                Ok(CborValue::Float(value))
            }
            31 => Err(CborError::BreakNotSupported),
            _ => Err(CborError::UnsupportedSimple),
        }
    }

    fn read_item(&mut self, depth: usize) -> Result<CborValue, CborError> {
        if depth > self.options.max_depth {
            return Err(CborError::DepthExceeded {
                limit: self.options.max_depth,
            });
        }
        let initial = self.read_byte()?;
        let major_type = initial >> 5;
        let additional_information = initial & 0x1f;
        match major_type {
            0 => Ok(CborValue::UInt(self.read_argument(additional_information)?)),
            1 => {
                let argument = self.read_argument(additional_information)?;
                let number = i64::try_from(-1i128 - i128::from(argument))
                    .map_err(|_| CborError::DecodedUnsafeInteger)?;
                Ok(CborValue::NInt(number))
            }
            2 => {
                let length = self.read_length(
                    additional_information,
                    "byte string",
                    self.options.max_byte_length,
                )?;
                Ok(CborValue::Bytes(self.read_bytes(length)?.to_vec()))
            }
            3 => {
                let length = self.read_length(
                    additional_information,
                    "text string",
                    self.options.max_byte_length,
                )?;
                let bytes = self.read_bytes(length)?;
                let text = std::str::from_utf8(bytes).map_err(|_| CborError::InvalidUtf8)?;
                Ok(CborValue::Text(text.to_owned()))
            }
            4 => {
                let length = self.read_length(
                    additional_information,
                    "array",
                    self.options.max_container_length,
                )?;
                let mut values = Vec::with_capacity(length);
                for _ in 0..length {
                    values.push(self.read_item(depth + 1)?);
                }
                Ok(CborValue::Array(values))
            }
            5 => {
                let length = self.read_length(
                    additional_information,
                    "map",
                    self.options.max_container_length,
                )?;
                let mut entries = Vec::with_capacity(length);
                let mut keys = std::collections::HashSet::with_capacity(length);
                for _ in 0..length {
                    let key = self.read_item(depth + 1)?;
                    let CborValue::Text(key) = key else {
                        return Err(CborError::NonStringKey);
                    };
                    if !keys.insert(key.clone()) {
                        return Err(CborError::DuplicateKey);
                    }
                    entries.push((key, self.read_item(depth + 1)?));
                }
                Ok(CborValue::Map(entries))
            }
            6 => Err(CborError::TagsNotSupported),
            7 => self.read_simple(additional_information),
            _ => Err(CborError::MalformedMajorType),
        }
    }
}

fn decode_cbor_value(bytes: &[u8], options: &CborOptions) -> Result<CborValue, CborError> {
    if bytes.len() > options.max_byte_length {
        return Err(CborError::ByteLengthExceeded {
            limit: options.max_byte_length,
        });
    }
    let mut decoder = CborDecoder::new(bytes, options);
    let value = decoder.read_item(0)?;
    if decoder.offset != bytes.len() {
        return Err(CborError::TrailingData);
    }
    Ok(value)
}

/// Error returned by a native protocol encode or decode operation.
#[derive(Debug, thiserror::Error)]
pub enum CodecError {
    /// Wrapped CBOR byte-level failure.
    #[error("CBOR error: {0}")]
    Cbor(#[from] CborError),
    /// Wrapped frame-layer failure.
    #[error("{0}")]
    Frame(#[from] FrameError),
    /// Client envelope failed validation.
    #[error("Invalid client protocol message: {0}")]
    InvalidClient(String),
    /// Server envelope failed validation.
    #[error("Invalid server protocol message: {0}")]
    InvalidServer(String),
    /// An unknown top-level message discriminant was received.
    #[error("Unknown discriminant: {0}")]
    UnknownDiscriminant(String),
    /// A server hello carried a version other than the protocol version.
    #[error("Protocol version mismatch: expected {expected}, got {got}")]
    VersionMismatch {
        /// Required protocol version.
        expected: u64,
        /// Received protocol version.
        got: u64,
    },
}

const CLIENT_DISCRIMINANTS: &[&str] = &["hello", "request", "cancel"];
const SERVER_DISCRIMINANTS: &[&str] = &[
    "hello",
    "hello_error",
    "response",
    "service_update",
    "attachment",
];

fn bounded_error_message(error: impl std::fmt::Display) -> String {
    let message = error.to_string();
    if message.chars().count() <= MAX_PROTOCOL_ERROR_CHARS {
        return message;
    }
    let mut bounded: String = message.chars().take(MAX_PROTOCOL_ERROR_CHARS - 3).collect();
    bounded.push_str("...");
    bounded
}

fn invalid_client(error: impl std::fmt::Display) -> CodecError {
    CodecError::InvalidClient(bounded_error_message(error))
}

fn invalid_server(error: impl std::fmt::Display) -> CodecError {
    CodecError::InvalidServer(bounded_error_message(error))
}

fn extract_discriminant(value: &CborValue, tag: &str) -> Option<String> {
    let CborValue::Map(entries) = value else {
        return None;
    };
    entries.iter().find_map(|(key, value)| {
        if key != tag {
            return None;
        }
        Some(match value {
            CborValue::Text(value) => bounded_error_message(value),
            other => bounded_error_message(format_args!("{other:?}")),
        })
    })
}

fn check_discriminant(
    value: &CborValue,
    tag: &str,
    allowed: &[&str],
    kind: &str,
) -> Result<(), CodecError> {
    if let Some(discriminant) = extract_discriminant(value, tag)
        && !allowed.contains(&discriminant.as_str())
    {
        return Err(CodecError::UnknownDiscriminant(format!(
            "{kind} discriminant `{discriminant}`"
        )));
    }
    Ok(())
}

fn validate_json_value(value: &JsonValue, depth: usize) -> Result<(), String> {
    if depth > MAX_JSON_DEPTH {
        return Err(format!("JSON nesting depth exceeds configured limit of {MAX_JSON_DEPTH}"));
    }
    match value {
        JsonValue::Null | JsonValue::Bool(_) => Ok(()),
        JsonValue::Number(number) => {
            if !number.is_finite() {
                return Err("JSON numbers must be finite".to_owned());
            }
            if number.fract() == 0.0 && number.abs() > MAX_SAFE_CBOR_INTEGER_F64 {
                return Err("JSON integer is outside the safe range".to_owned());
            }
            Ok(())
        }
        JsonValue::String(value) => value
            .try_to_utf8()
            .map(|_| ())
            .map_err(|_| "CBOR text strings must contain valid Unicode scalar values".to_owned()),
        JsonValue::Array(values) => {
            for value in values {
                validate_json_value(value, depth + 1)?;
            }
            Ok(())
        }
        JsonValue::Object(entries) => {
            for (key, value) in entries {
                key.try_to_utf8().map_err(|_| {
                    "CBOR text strings must contain valid Unicode scalar values".to_owned()
                })?;
                validate_json_value(value, depth + 1)?;
            }
            Ok(())
        }
    }
}

fn validate_id(id: &str, field: &str) -> Result<(), String> {
    if id.is_empty() {
        Err(format!("{field} must be non-empty"))
    } else {
        Ok(())
    }
}

fn validate_protocol_error(error: &ProtocolError) -> Result<(), String> {
    validate_id(&error.code, "error code")
}

fn validate_server_id(server_id: &ServerId) -> Result<(), String> {
    if is_server_id(server_id.as_str()) {
        Ok(())
    } else {
        Err("serverId is not a lowercase UUIDv4".to_owned())
    }
}

fn validate_session_target(target: &SessionTarget) -> Result<(), String> {
    validate_server_id(&target.server_id)?;
    validate_id(&target.session_id, "sessionId")?;
    validate_id(&target.attachment_id, "attachmentId")
}

fn validate_target(target: &RpcTarget) -> Result<(), String> {
    match target {
        RpcTarget::Server(ServerTarget { server_id }) => validate_server_id(server_id),
        RpcTarget::Session(target) => validate_session_target(target),
    }
}

fn validate_client_message(message: &ClientMessage) -> Result<(), CodecError> {
    match message {
        ClientMessage::Hello { .. } => Ok(()),
        ClientMessage::Request { id, target, call } => {
            validate_id(id, "request id").map_err(invalid_client)?;
            validate_target(target).map_err(invalid_client)?;
            validate_json_value(call, 0).map_err(invalid_client)
        }
        ClientMessage::Cancel { id, target } => {
            validate_id(id, "cancel id").map_err(invalid_client)?;
            validate_target(target).map_err(invalid_client)
        }
    }
}

fn validate_server_message(message: &ServerMessage) -> Result<(), CodecError> {
    match message {
        ServerMessage::Hello { version, server_id } => {
            if *version != PROTOCOL_VERSION {
                return Err(CodecError::VersionMismatch {
                    expected: PROTOCOL_VERSION,
                    got: *version,
                });
            }
            validate_server_id(server_id).map_err(invalid_server)
        }
        ServerMessage::HelloError { error } => {
            validate_protocol_error(error).map_err(invalid_server)
        }
        ServerMessage::Response { id, result } => {
            validate_id(id, "response id").map_err(invalid_server)?;
            if let Some(result) = result {
                validate_json_value(result, 0).map_err(invalid_server)?;
            }
            Ok(())
        }
        ServerMessage::ResponseError { id, error } => {
            validate_id(id, "response id").map_err(invalid_server)?;
            validate_protocol_error(error).map_err(invalid_server)
        }
        ServerMessage::ServiceUpdate {
            subscription_id,
            update,
        } => {
            validate_id(subscription_id, "subscriptionId").map_err(invalid_server)?;
            validate_json_value(update, 0).map_err(invalid_server)
        }
        ServerMessage::Attachment { attachment } => {
            if let Some(target) = attachment {
                validate_session_target(target).map_err(invalid_server)?;
            }
            Ok(())
        }
    }
}

fn validate_cbor_value(value: &CborValue, depth: usize) -> Result<(), CborError> {
    if depth > DEFAULT_MAX_CBOR_DEPTH {
        return Err(CborError::DepthExceeded {
            limit: DEFAULT_MAX_CBOR_DEPTH,
        });
    }
    match value {
        CborValue::Null | CborValue::Bool(_) | CborValue::Text(_) | CborValue::Bytes(_) => Ok(()),
        CborValue::UInt(number) => {
            if *number > MAX_SAFE_CBOR_INTEGER {
                Err(CborError::UnsafeInteger)
            } else {
                Ok(())
            }
        }
        CborValue::NInt(number) => {
            if *number < -(MAX_SAFE_CBOR_INTEGER as i64) {
                Err(CborError::UnsafeInteger)
            } else {
                Ok(())
            }
        }
        CborValue::Float(number) => {
            if !number.is_finite() {
                return Err(CborError::NonFinite);
            }
            if number.fract() == 0.0 && number.abs() > MAX_SAFE_CBOR_INTEGER_F64 {
                return Err(CborError::UnsafeInteger);
            }
            Ok(())
        }
        CborValue::Array(values) => {
            if values.len() > DEFAULT_MAX_CBOR_CONTAINER_LENGTH {
                return Err(CborError::ArrayLengthExceeded {
                    limit: DEFAULT_MAX_CBOR_CONTAINER_LENGTH,
                });
            }
            for value in values {
                validate_cbor_value(value, depth + 1)?;
            }
            Ok(())
        }
        CborValue::Map(entries) => {
            if entries.len() > DEFAULT_MAX_CBOR_CONTAINER_LENGTH {
                return Err(CborError::MapLengthExceeded {
                    limit: DEFAULT_MAX_CBOR_CONTAINER_LENGTH,
                });
            }
            let mut keys = std::collections::HashSet::with_capacity(entries.len());
            for (key, value) in entries {
                if !keys.insert(key) {
                    return Err(CborError::DuplicateKey);
                }
                validate_cbor_value(value, depth + 1)?;
            }
            Ok(())
        }
    }
}

fn decode_client_payload(payload: &[u8], options: &CborOptions) -> Result<ClientMessage, CodecError> {
    let value = decode_cbor_value(payload, options)?;
    check_discriminant(&value, "type", CLIENT_DISCRIMINANTS, "client")?;
    let message = ClientMessage::deserialize(CborValueDeserializer { value })
        .map_err(|error: SerError| {
            if error.0.contains("unknown variant") {
                CodecError::UnknownDiscriminant(bounded_error_message(error))
            } else {
                invalid_client(error)
            }
        })?;
    validate_client_message(&message)?;
    Ok(message)
}

fn decode_server_payload(payload: &[u8], options: &CborOptions) -> Result<ServerMessage, CodecError> {
    let value = decode_cbor_value(payload, options)?;
    check_discriminant(&value, "type", SERVER_DISCRIMINANTS, "server")?;
    let message = ServerMessage::deserialize(CborValueDeserializer { value })
        .map_err(|error: SerError| {
            if error.0.contains("unknown variant") {
                CodecError::UnknownDiscriminant(bounded_error_message(error))
            } else {
                invalid_server(error)
            }
        })?;
    validate_server_message(&message)?;
    Ok(message)
}

/// Returns whether a client-offered version is the supported v8 version.
#[must_use]
pub fn is_supported_protocol_version(version: u64) -> bool {
    version == PROTOCOL_VERSION
}

/// Encodes one complete v8 client message in a length-prefixed frame.
///
/// # Errors
///
/// Returns [`CodecError::InvalidClient`] for a schema violation,
/// [`CodecError::Cbor`] for a CBOR violation, or [`CodecError::Frame`] when
/// the configured frame limit is invalid or exceeded.
pub fn encode_client_message(
    message: &ClientMessage,
    options: Option<FrameDecoderOptions>,
) -> Result<Vec<u8>, CodecError> {
    validate_client_message(message)?;
    let cbor_options = CborOptions::from_frame(options);
    let value = message
        .serialize(CborValueSerializer)
        .map_err(invalid_client)?;
    validate_cbor_value(&value, 0)?;
    let payload = encode_cbor_value(&value, &cbor_options)?;
    let frame = encode_frame(&payload);
    assert_complete_frame(&frame, options)?;
    Ok(frame)
}

/// Encodes one complete v8 server message in a length-prefixed frame.
///
/// # Errors
///
/// Returns [`CodecError::InvalidServer`] for a schema violation,
/// [`CodecError::Cbor`] for a CBOR violation, or [`CodecError::Frame`] when
/// the configured frame limit is invalid or exceeded.
pub fn encode_server_message(
    message: &ServerMessage,
    options: Option<FrameDecoderOptions>,
) -> Result<Vec<u8>, CodecError> {
    validate_server_message(message)?;
    let cbor_options = CborOptions::from_frame(options);
    let value = message
        .serialize(CborValueSerializer)
        .map_err(invalid_server)?;
    validate_cbor_value(&value, 0)?;
    let payload = encode_cbor_value(&value, &cbor_options)?;
    let frame = encode_frame(&payload);
    assert_complete_frame(&frame, options)?;
    Ok(frame)
}

/// Decodes one complete framed v8 client message.
///
/// # Errors
///
/// Returns [`CodecError::Frame`] for malformed framing, [`CodecError::Cbor`]
/// for malformed CBOR, or a validation variant for an invalid envelope.
pub fn decode_client_message(
    frame: &[u8],
    options: Option<FrameDecoderOptions>,
) -> Result<ClientMessage, CodecError> {
    assert_complete_frame(frame, options)?;
    let payload = frame
        .get(FRAME_HEADER_LENGTH..)
        .ok_or_else(|| invalid_client("frame payload is missing"))?;
    decode_client_payload(payload, &CborOptions::from_frame(options))
}

/// Decodes one complete framed v8 server message.
///
/// # Errors
///
/// Returns [`CodecError::Frame`] for malformed framing, [`CodecError::Cbor`]
/// for malformed CBOR, or a validation variant for an invalid envelope.
pub fn decode_server_message(
    frame: &[u8],
    options: Option<FrameDecoderOptions>,
) -> Result<ServerMessage, CodecError> {
    assert_complete_frame(frame, options)?;
    let payload = frame
        .get(FRAME_HEADER_LENGTH..)
        .ok_or_else(|| invalid_server("frame payload is missing"))?;
    decode_server_payload(payload, &CborOptions::from_frame(options))
}

/// Incremental decoder for framed client messages.
pub struct ClientMessageDecoder {
    frames: FrameDecoder,
    options: Option<FrameDecoderOptions>,
    failed: bool,
}

impl ClientMessageDecoder {
    /// Creates a client decoder with optional frame limits.
    ///
    /// # Errors
    ///
    /// Returns [`FrameError::InvalidLimit`] for an invalid limit.
    pub fn new(options: Option<FrameDecoderOptions>) -> Result<Self, FrameError> {
        Ok(Self {
            frames: FrameDecoder::new(options)?,
            options,
            failed: false,
        })
    }

    /// Feeds arbitrary bytes and returns every complete decoded message.
    ///
    /// Any frame, CBOR, or schema error permanently fails this decoder.
    pub fn push(&mut self, chunk: &[u8]) -> Result<Vec<ClientMessage>, CodecError> {
        if self.failed {
            return Err(invalid_client("client message decoder has failed"));
        }
        let result = self.push_inner(chunk);
        match result {
            Ok(messages) => Ok(messages),
            Err(error) => {
                self.failed = true;
                Err(error)
            }
        }
    }

    fn push_inner(&mut self, chunk: &[u8]) -> Result<Vec<ClientMessage>, CodecError> {
        let payloads = self.frames.push(chunk)?;
        let options = CborOptions::from_frame(self.options);
        let mut messages = Vec::with_capacity(payloads.len());
        for payload in payloads {
            messages.push(decode_client_payload(&payload, &options)?);
        }
        Ok(messages)
    }

    /// Ends the input stream, rejecting a partial frame and latching failure.
    pub fn end(&mut self) -> Result<(), CodecError> {
        if self.failed {
            return Err(invalid_client("client message decoder has failed"));
        }
        match self.frames.end() {
            Ok(()) => Ok(()),
            Err(error) => {
                self.failed = true;
                Err(CodecError::Frame(error))
            }
        }
    }
}

/// Incremental decoder for framed server messages.
pub struct ServerMessageDecoder {
    frames: FrameDecoder,
    options: Option<FrameDecoderOptions>,
    failed: bool,
}

impl ServerMessageDecoder {
    /// Creates a server decoder with optional frame limits.
    ///
    /// # Errors
    ///
    /// Returns [`FrameError::InvalidLimit`] for an invalid limit.
    pub fn new(options: Option<FrameDecoderOptions>) -> Result<Self, FrameError> {
        Ok(Self {
            frames: FrameDecoder::new(options)?,
            options,
            failed: false,
        })
    }

    /// Feeds arbitrary bytes and returns every complete decoded message.
    ///
    /// Any frame, CBOR, or schema error permanently fails this decoder.
    pub fn push(&mut self, chunk: &[u8]) -> Result<Vec<ServerMessage>, CodecError> {
        if self.failed {
            return Err(invalid_server("server message decoder has failed"));
        }
        let result = self.push_inner(chunk);
        match result {
            Ok(messages) => Ok(messages),
            Err(error) => {
                self.failed = true;
                Err(error)
            }
        }
    }

    fn push_inner(&mut self, chunk: &[u8]) -> Result<Vec<ServerMessage>, CodecError> {
        let payloads = self.frames.push(chunk)?;
        let options = CborOptions::from_frame(self.options);
        let mut messages = Vec::with_capacity(payloads.len());
        for payload in payloads {
            messages.push(decode_server_payload(&payload, &options)?);
        }
        Ok(messages)
    }

    /// Ends the input stream, rejecting a partial frame and latching failure.
    pub fn end(&mut self) -> Result<(), CodecError> {
        if self.failed {
            return Err(invalid_server("server message decoder has failed"));
        }
        match self.frames.end() {
            Ok(()) => Ok(()),
            Err(error) => {
                self.failed = true;
                Err(CodecError::Frame(error))
            }
        }
    }
}

/// Creates a framed client-message decoder.
///
/// # Errors
///
/// Returns [`FrameError::InvalidLimit`] for an invalid limit.
pub fn create_client_message_decoder(
    options: Option<FrameDecoderOptions>,
) -> Result<ClientMessageDecoder, FrameError> {
    ClientMessageDecoder::new(options)
}

/// Creates a framed server-message decoder.
///
/// # Errors
///
/// Returns [`FrameError::InvalidLimit`] for an invalid limit.
pub fn create_server_message_decoder(
    options: Option<FrameDecoderOptions>,
) -> Result<ServerMessageDecoder, FrameError> {
    ServerMessageDecoder::new(options)
}

#[cfg(test)]
mod tests {
    use super::*;
    use pi_agent::service::value::{JsString, JsonValue, parse_json};

    fn server_id() -> crate::remote::schemas::ServerId {
        crate::remote::schemas::ServerId::new("00000000-0000-4000-8000-000000000001")
            .expect("valid test server id")
    }

    fn request() -> ClientMessage {
        ClientMessage::Request {
            id: "request-1".to_owned(),
            target: RpcTarget::Session(SessionTarget {
                server_id: server_id(),
                session_id: "session-1".to_owned(),
                attachment_id: "attachment-1".to_owned(),
            }),
            call: parse_json(r#"{"serviceId":"application.custom","args":[null,true]}"#)
                .expect("valid test JSON"),
        }
    }

    #[test]
    fn roundtrips_opaque_request_and_v8_hello() {
        let hello = encode_client_message(
            &ClientMessage::Hello {
                version: PROTOCOL_VERSION,
            },
            None,
        )
        .expect("encode hello");
        assert!(matches!(
            decode_client_message(&hello, None).expect("decode hello"),
            ClientMessage::Hello { version: 8 }
        ));

        let frame = encode_client_message(&request(), None).expect("encode request");
        assert_eq!(decode_client_message(&frame, None).expect("decode request"), request());
    }

    #[test]
    fn response_absence_and_null_remain_distinct_on_wire() {
        let absent = ServerMessage::Response {
            id: "request-1".to_owned(),
            result: None,
        };
        let explicit_null = ServerMessage::Response {
            id: "request-1".to_owned(),
            result: Some(JsonValue::Null),
        };
        let absent = decode_server_message(
            &encode_server_message(&absent, None).expect("encode absent"),
            None,
        )
        .expect("decode absent");
        let explicit_null = decode_server_message(
            &encode_server_message(&explicit_null, None).expect("encode null"),
            None,
        )
        .expect("decode null");
        assert!(matches!(absent, ServerMessage::Response { result: None, .. }));
        assert!(matches!(
            explicit_null,
            ServerMessage::Response {
                result: Some(JsonValue::Null),
                ..
            }
        ));
    }

    #[test]
    fn encode_rejects_lone_surrogate_in_opaque_value() {
        let message = ClientMessage::Request {
            id: "request-1".to_owned(),
            target: RpcTarget::Server(ServerTarget {
                server_id: server_id(),
            }),
            call: JsonValue::String(JsString::from_utf16(vec![0xd800])),
        };
        let error = encode_client_message(&message, None).expect_err("surrogate must be rejected");
        match error {
            CodecError::InvalidClient(message) => {
                assert!(message.contains("valid Unicode scalar values"));
            }
            other => panic!("expected InvalidClient, got {other:?}"),
        }
    }

    #[test]
    fn decode_rejects_byte_string_in_opaque_value() {
        let mut payload = vec![
            0xa4, // map(4)
            0x64, b't', b'y', b'p', b'e',
            0x67, b'r', b'e', b'q', b'u', b'e', b's', b't',
            0x62, b'i', b'd',
            0x69, b'r', b'e', b'q', b'u', b'e', b's', b't', b'-', b'1',
            0x66, b't', b'a', b'r', b'g', b'e', b't',
            0xa1, // map(1)
            0x68, b's', b'e', b'r', b'v', b'e', b'r', b'I', b'd',
            0x78, 0x24,
        ];
        payload.extend_from_slice(b"00000000-0000-4000-8000-000000000001");
        payload.extend_from_slice(&[0x64, b'c', b'a', b'l', b'l', 0x43, 1, 2, 3]);
        let frame = encode_frame(&payload);
        let error = decode_client_message(&frame, None).expect_err("byte string must be rejected");
        match error {
            CodecError::InvalidClient(message) => {
                assert!(message.contains("byte strings are not permitted"));
            }
            other => panic!("expected InvalidClient, got {other:?}"),
        }
    }
    #[test]
    fn encode_rejects_unsafe_integral_json_number() {
        let message = ClientMessage::Request {
            id: "request-1".to_owned(),
            target: RpcTarget::Server(ServerTarget {
                server_id: server_id(),
            }),
            call: JsonValue::Number(MAX_SAFE_CBOR_INTEGER_F64 + 1.0),
        };
        let error = encode_client_message(&message, None).expect_err("unsafe integer must fail");
        match error {
            CodecError::InvalidClient(message) => {
                assert!(message.contains("outside the safe range"));
            }
            other => panic!("expected InvalidClient, got {other:?}"),
        }
    }

    #[test]
    fn decode_rejects_non_string_cbor_map_key() {
        let frame = encode_frame(&[0xa1, 0x01, 0x00]);
        let error = decode_client_message(&frame, None).expect_err("numeric key must fail");
        assert!(matches!(
            error,
            CodecError::Cbor(CborError::NonStringKey)
        ));
    }

    #[test]
    fn response_negative_zero_roundtrips_with_sign() {
        let message = ServerMessage::Response {
            id: "request-1".to_owned(),
            result: Some(JsonValue::Number(-0.0)),
        };
        let decoded = decode_server_message(
            &encode_server_message(&message, None).expect("encode response"),
            None,
        )
        .expect("decode response");
        let ServerMessage::Response {
            result: Some(JsonValue::Number(value)),
            ..
        } = &decoded
        else {
            panic!("expected response with numeric result");
        };
        assert!(*value == 0.0 && value.is_sign_negative());
    }

    #[test]
    fn integral_binary64_uses_cbor_integer_encoding() {
        let message = ServerMessage::Response {
            id: "request-1".to_owned(),
            result: Some(JsonValue::Number(1.0)),
        };
        let frame = encode_server_message(&message, None).expect("encode response");
        assert!(frame.ends_with(&[0x66, b'r', b'e', b's', b'u', b'l', b't', 0x01]));
    }

    #[test]
    fn incremental_decoder_latches_after_schema_failure() {
        let invalid = encode_frame(&[0xa2, 0x64, b't', b'y', b'p', b'e', 0x65, b'h', b'e', b'l', b'l', b'o', 0x67, b'e', b'x', b't', b'r', b'a', 0xf5]);
        let mut decoder = ClientMessageDecoder::new(None).expect("decoder");
        assert!(decoder.push(&invalid).is_err());
        assert!(decoder
            .push(&encode_client_message(&ClientMessage::Hello { version: 8 }, None).expect("hello"))
            .is_err());
    }

    #[test]
    fn server_hello_requires_v8_and_canonical_server_id() {
        let wrong_version = ServerMessage::Hello {
            version: 7,
            server_id: server_id(),
        };
        assert!(matches!(
            encode_server_message(&wrong_version, None),
            Err(CodecError::VersionMismatch { expected: 8, got: 7 })
        ));
    }
}
