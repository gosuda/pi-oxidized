//! Strict RFC 8949 CBOR codec + message encode/decode — portable port of
//! upstream `cbor/*` and `codec.ts`.
//!
//! All failures surface as typed [`CodecError`]; this layer never panics.

use serde::{Deserialize, Serialize};
use thiserror::Error;

use crate::remote::framing::{
    DEFAULT_MAX_FRAME_LENGTH, FrameDecoder, FrameDecoderOptions, FrameError, assert_complete_frame,
    encode_frame,
};
use crate::remote::schemas::{ClientMessage, PROTOCOL_VERSION, ServerMessage};
use crate::remote::serde_cbor::{CborValue, CborValueDeserializer, CborValueSerializer};

// ---------------------------------------------------------------------------
// CBOR options (private)
// ---------------------------------------------------------------------------

const DEFAULT_MAX_CBOR_CONTAINER_LENGTH: usize = 1_000_000;
const DEFAULT_MAX_CBOR_DEPTH: usize = 64;

/// Configuration limits for CBOR encoding and decoding.
#[derive(Debug, Clone, Copy)]
#[expect(
    clippy::struct_field_names,
    reason = "all limits are maxima; prefix is intentional"
)]
struct CborOptions {
    /// Maximum total byte length for a CBOR payload.
    max_byte_length: usize,
    /// Maximum number of elements in an array or map.
    max_container_length: usize,
    /// Maximum nesting depth.
    max_depth: usize,
}

impl CborOptions {
    fn from_frame(options: Option<FrameDecoderOptions>) -> Self {
        Self {
            max_byte_length: options.map_or(DEFAULT_MAX_FRAME_LENGTH, |o| o.max_frame_length),
            max_container_length: DEFAULT_MAX_CBOR_CONTAINER_LENGTH,
            max_depth: DEFAULT_MAX_CBOR_DEPTH,
        }
    }
}

// ---------------------------------------------------------------------------
// CBOR byte-level error (exposed via CodecError::Cbor)
// ---------------------------------------------------------------------------

/// Byte-level CBOR encoding or decoding error.
#[derive(Debug, Clone, PartialEq, Error)]
pub enum CborError {
    /// CBOR byte length exceeds the configured limit.
    #[error("byte length {limit} exceeds configured limit")]
    ByteLengthExceeded {
        /// Maximum byte length configured when the error was raised.
        limit: usize,
    },
    /// CBOR text string length exceeds the configured limit.
    #[error("text length {limit} exceeds configured limit")]
    TextLengthExceeded {
        /// Maximum text length configured when the error was raised.
        limit: usize,
    },
    /// CBOR byte string length exceeds the configured limit.
    #[error("byte-string length {limit} exceeds configured limit")]
    ByteStringLengthExceeded {
        /// Maximum byte-string length configured when the error was raised.
        limit: usize,
    },
    /// CBOR array length exceeds the configured limit.
    #[error("array length {limit} exceeds configured limit")]
    ArrayLengthExceeded {
        /// Maximum array length configured when the error was raised.
        limit: usize,
    },
    /// CBOR map length exceeds the configured limit.
    #[error("map length {limit} exceeds configured limit")]
    MapLengthExceeded {
        /// Maximum map length configured when the error was raised.
        limit: usize,
    },
    /// CBOR nesting depth exceeds the configured limit.
    #[error("nesting depth {limit} exceeds configured limit")]
    DepthExceeded {
        /// Maximum nesting depth configured when the error was raised.
        limit: usize,
    },
    /// CBOR numbers must be finite (NaN/Infinity rejected).
    #[error("CBOR numbers must be finite")]
    NonFinite,
    /// CBOR payload was truncated mid-item.
    #[error("Truncated CBOR payload")]
    Truncated,
    /// CBOR payload has trailing bytes after the top-level item.
    #[error("CBOR payload contains trailing data")]
    TrailingData,
    /// CBOR tags (major type 6) are not supported.
    #[error("CBOR tags are not supported")]
    TagsNotSupported,
    /// CBOR break marker (0xFF) is not supported.
    #[error("CBOR break marker is not supported")]
    BreakNotSupported,
    /// Indefinite-length CBOR items are not supported.
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
    /// Decoded CBOR integer is outside the safe `i64` range.
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

// ---------------------------------------------------------------------------
// CBOR byte-level encoder (port of upstream encoder.ts)
// ---------------------------------------------------------------------------

struct CborEncoder<'a> {
    buf: Vec<u8>,
    options: &'a CborOptions,
}

impl<'a> CborEncoder<'a> {
    fn new(options: &'a CborOptions) -> Self {
        Self {
            buf: Vec::with_capacity(256),
            options,
        }
    }
    fn finish(self) -> Vec<u8> {
        self.buf
    }
    fn write_byte(&mut self, b: u8) {
        self.buf.push(b);
    }
    fn write_bytes(&mut self, b: &[u8]) {
        self.buf.extend_from_slice(b);
    }

    #[expect(
        clippy::cast_possible_truncation,
        reason = "bounded by preceding <= 0xff/0xffff/0xffff_ffff checks"
    )]
    fn write_argument(&mut self, major_type: u8, value: u64) {
        let p = major_type << 5;
        if value < 24 {
            self.write_byte(p | value as u8);
        } else if value <= 0xff {
            self.write_byte(p | 0x18);
            self.write_byte(value as u8);
        } else if value <= 0xffff {
            self.write_byte(p | 0x19);
            self.buf.extend_from_slice(&(value as u16).to_be_bytes());
        } else if value <= 0xffff_ffff {
            self.write_byte(p | 0x1a);
            self.buf.extend_from_slice(&(value as u32).to_be_bytes());
        } else {
            self.write_byte(p | 0x1b);
            self.buf.extend_from_slice(&value.to_be_bytes());
        }
    }

    fn encode_text(&mut self, s: &str) -> Result<(), CborError> {
        let b = s.as_bytes();
        if b.len() > self.options.max_byte_length {
            return Err(CborError::TextLengthExceeded {
                limit: self.options.max_byte_length,
            });
        }
        self.write_argument(3, b.len() as u64);
        self.write_bytes(b);
        Ok(())
    }

    #[expect(
        clippy::cast_sign_loss,
        reason = "CBOR negative integers are encoded as -1-n in u64 space"
    )]
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
            CborValue::UInt(n) => self.write_argument(0, *n),
            CborValue::NInt(n) => self.write_argument(1, (-1i64).wrapping_sub(*n) as u64),
            CborValue::Float(f) => {
                if !f.is_finite() {
                    return Err(CborError::NonFinite);
                }
                self.write_byte(0xfb);
                self.buf.extend_from_slice(&f.to_be_bytes());
            }
            CborValue::Text(s) => self.encode_text(s)?,
            CborValue::Bytes(b) => {
                if b.len() > self.options.max_byte_length {
                    return Err(CborError::ByteStringLengthExceeded {
                        limit: self.options.max_byte_length,
                    });
                }
                self.write_argument(2, b.len() as u64);
                self.write_bytes(b);
            }
            CborValue::Array(arr) => {
                if arr.len() > self.options.max_container_length {
                    return Err(CborError::ArrayLengthExceeded {
                        limit: self.options.max_container_length,
                    });
                }
                self.write_argument(4, arr.len() as u64);
                for item in arr {
                    self.encode_value(item, depth + 1)?;
                }
            }
            CborValue::Map(entries) => {
                if entries.len() > self.options.max_container_length {
                    return Err(CborError::MapLengthExceeded {
                        limit: self.options.max_container_length,
                    });
                }
                self.write_argument(5, entries.len() as u64);
                for (k, v) in entries {
                    self.encode_text(k)?;
                    self.encode_value(v, depth + 1)?;
                }
            }
        }
        Ok(())
    }
}

fn encode_cbor_value(value: &CborValue, options: &CborOptions) -> Result<Vec<u8>, CborError> {
    let mut enc = CborEncoder::new(options);
    enc.encode_value(value, 0)?;
    let out = enc.finish();
    if out.len() > options.max_byte_length {
        return Err(CborError::ByteLengthExceeded {
            limit: options.max_byte_length,
        });
    }
    Ok(out)
}

// ---------------------------------------------------------------------------
// CBOR byte-level decoder (port of upstream decoder.ts)
// ---------------------------------------------------------------------------

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
        if self.offset >= self.bytes.len() {
            return Err(CborError::Truncated);
        }
        let v = self.bytes[self.offset];
        self.offset += 1;
        Ok(v)
    }
    fn read_bytes(&mut self, len: usize) -> Result<&'a [u8], CborError> {
        if len > self.bytes.len() - self.offset {
            return Err(CborError::Truncated);
        }
        let v = &self.bytes[self.offset..self.offset + len];
        self.offset += len;
        Ok(v)
    }
    fn read_argument(&mut self, ai: u8) -> Result<u64, CborError> {
        match ai {
            0..=23 => Ok(u64::from(ai)),
            24 => Ok(u64::from(self.read_byte()?)),
            25 => {
                let b = self.read_bytes(2)?;
                Ok(u64::from(u16::from_be_bytes([b[0], b[1]])))
            }
            26 => {
                let b = self.read_bytes(4)?;
                Ok(u64::from(u32::from_be_bytes([b[0], b[1], b[2], b[3]])))
            }
            27 => {
                let b = self.read_bytes(8)?;
                Ok(u64::from_be_bytes([
                    b[0], b[1], b[2], b[3], b[4], b[5], b[6], b[7],
                ]))
            }
            31 => Err(CborError::IndefiniteLength("item")),
            _ => Err(CborError::MalformedAdditionalInfo),
        }
    }
    #[expect(
        clippy::cast_possible_truncation,
        reason = "bounded by preceding len > limit check; usize is >= 32 bits on all targets"
    )]
    fn read_length(
        &mut self,
        ai: u8,
        kind: &'static str,
        limit: usize,
    ) -> Result<usize, CborError> {
        if ai == 31 {
            return Err(CborError::IndefiniteLength(kind));
        }
        let len = self.read_argument(ai)?;
        if len > limit as u64 {
            return Err(CborError::MapLengthExceeded { limit });
        }
        Ok(len as usize)
    }
    fn read_simple(&mut self, ai: u8) -> Result<CborValue, CborError> {
        match ai {
            20 => Ok(CborValue::Bool(false)),
            21 => Ok(CborValue::Bool(true)),
            22 => Ok(CborValue::Null),
            27 => {
                let b = self.read_bytes(8)?;
                let f = f64::from_be_bytes([b[0], b[1], b[2], b[3], b[4], b[5], b[6], b[7]]);
                if !f.is_finite() {
                    return Err(CborError::DecodedNonFinite);
                }
                Ok(CborValue::Float(f))
            }
            31 => Err(CborError::BreakNotSupported),
            _ => Err(CborError::UnsupportedSimple),
        }
    }
    #[expect(
        clippy::cast_possible_wrap,
        reason = "bounded by preceding n > i64::MAX check"
    )]
    fn read_item(&mut self, depth: usize) -> Result<CborValue, CborError> {
        if depth > self.options.max_depth {
            return Err(CborError::DepthExceeded {
                limit: self.options.max_depth,
            });
        }
        let initial = self.read_byte()?;
        let major = initial >> 5;
        let ai = initial & 0x1f;
        match major {
            0 => Ok(CborValue::UInt(self.read_argument(ai)?)),
            1 => {
                let n = self.read_argument(ai)?;
                if n > i64::MAX as u64 {
                    return Err(CborError::DecodedUnsafeInteger);
                }
                Ok(CborValue::NInt((-1i64).wrapping_sub(n as i64)))
            }
            2 => {
                let len = self.read_length(ai, "byte string", self.options.max_byte_length)?;
                Ok(CborValue::Bytes(self.read_bytes(len)?.to_vec()))
            }
            3 => {
                let len = self.read_length(ai, "text string", self.options.max_byte_length)?;
                let b = self.read_bytes(len)?;
                match std::str::from_utf8(b) {
                    Ok(s) => Ok(CborValue::Text(s.to_string())),
                    Err(_) => Err(CborError::InvalidUtf8),
                }
            }
            4 => {
                let len = self.read_length(ai, "array", self.options.max_container_length)?;
                let mut a = Vec::with_capacity(len);
                for _ in 0..len {
                    a.push(self.read_item(depth + 1)?);
                }
                Ok(CborValue::Array(a))
            }
            5 => {
                let len = self.read_length(ai, "map", self.options.max_container_length)?;
                let mut e = Vec::with_capacity(len);
                let mut seen = std::collections::HashSet::with_capacity(len);
                for _ in 0..len {
                    let k = self.read_item(depth + 1)?;
                    let CborValue::Text(ks) = k else {
                        return Err(CborError::NonStringKey);
                    };
                    if !seen.insert(ks.clone()) {
                        return Err(CborError::DuplicateKey);
                    }
                    e.push((ks, self.read_item(depth + 1)?));
                }
                Ok(CborValue::Map(e))
            }
            6 => Err(CborError::TagsNotSupported),
            7 => self.read_simple(ai),
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
    let mut d = CborDecoder::new(bytes, options);
    let v = d.read_item(0)?;
    if d.offset != bytes.len() {
        return Err(CborError::TrailingData);
    }
    Ok(v)
}

// ---------------------------------------------------------------------------
// Public codec error
// ---------------------------------------------------------------------------

/// Error returned by any codec operation (CBOR encode/decode or framing).
#[derive(Debug, thiserror::Error)]
pub enum CodecError {
    /// Wrapped CBOR byte-level error.
    #[error("CBOR error: {0}")]
    Cbor(#[from] CborError),
    /// Wrapped frame-layer error.
    #[error("{0}")]
    Frame(#[from] FrameError),
    /// Client message failed validation.
    #[error("Invalid client protocol message: {0}")]
    InvalidClient(String),
    /// Server message failed validation.
    #[error("Invalid server protocol message: {0}")]
    InvalidServer(String),
    /// Protocol version mismatch between client and server.
    #[error("Protocol version mismatch: expected {expected}, got {got}")]
    VersionMismatch {
        /// Expected protocol version.
        expected: u64,
        /// Actual protocol version received.
        got: u64,
    },
    /// Unknown message discriminant encountered.
    #[error("Unknown discriminant: {0}")]
    UnknownDiscriminant(String),
}

// ---------------------------------------------------------------------------
// Discriminant checking
// ---------------------------------------------------------------------------

const CLIENT_DISCRIMINANTS: &[&str] = &["hello", "request"];
const SERVER_DISCRIMINANTS: &[&str] = &["hello", "hello_error", "response", "event"];

fn extract_discriminant(value: &CborValue, tag: &str) -> Option<String> {
    if let CborValue::Map(entries) = value {
        for (key, val) in entries {
            if key == tag {
                return match val {
                    CborValue::Text(s) => Some(s.clone()),
                    _ => Some(format!("{val:?}")),
                };
            }
        }
    }
    None
}

fn check_discriminant(
    value: &CborValue,
    tag: &str,
    allowed: &[&str],
    kind: &str,
) -> Result<(), CodecError> {
    if let Some(disc) = extract_discriminant(value, tag)
        && !allowed.contains(&disc.as_str())
    {
        return Err(CodecError::UnknownDiscriminant(format!(
            "{kind} discriminant `{disc}`"
        )));
    }
    Ok(())
}

fn check_client_discriminant(value: &CborValue) -> Result<(), CodecError> {
    check_discriminant(value, "type", CLIENT_DISCRIMINANTS, "client")
}

fn check_server_discriminant(value: &CborValue) -> Result<(), CodecError> {
    check_discriminant(value, "type", SERVER_DISCRIMINANTS, "server")
}

fn validate_client_message(msg: &ClientMessage) -> Result<(), CodecError> {
    if let ClientMessage::Hello { version } = msg
        && *version != PROTOCOL_VERSION
    {
        return Err(CodecError::VersionMismatch {
            expected: u64::from(PROTOCOL_VERSION),
            got: u64::from(*version),
        });
    }
    Ok(())
}

fn validate_server_message(msg: &ServerMessage) -> Result<(), CodecError> {
    if let ServerMessage::Hello { version, .. } = msg
        && *version != PROTOCOL_VERSION
    {
        return Err(CodecError::VersionMismatch {
            expected: u64::from(PROTOCOL_VERSION),
            got: u64::from(*version),
        });
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Public API
// ---------------------------------------------------------------------------

/// Returns `true` if the given protocol version is supported.
#[must_use]
pub fn is_supported_protocol_version(version: u64) -> bool {
    version == u64::from(PROTOCOL_VERSION)
}

/// Encodes a [`ClientMessage`] into a framed byte buffer.
///
/// # Errors
///
/// Returns [`CodecError::InvalidClient`] if serialization fails,
/// [`CodecError::Cbor`] if CBOR encoding fails, or
/// [`CodecError::Frame`] if the frame exceeds the configured limit.
pub fn encode_client_message(
    msg: &ClientMessage,
    options: Option<FrameDecoderOptions>,
) -> Result<Vec<u8>, CodecError> {
    let opts = CborOptions::from_frame(options);
    let value = msg
        .serialize(CborValueSerializer)
        .map_err(|e| CodecError::InvalidClient(e.to_string()))?;
    let payload = encode_cbor_value(&value, &opts)?;
    let frame = encode_frame(&payload);
    assert_complete_frame(&frame, options)?;
    Ok(frame)
}

/// Encodes a [`ServerMessage`] into a framed byte buffer.
///
/// # Errors
///
/// Returns [`CodecError::InvalidServer`] if serialization fails,
/// [`CodecError::Cbor`] if CBOR encoding fails, or
/// [`CodecError::Frame`] if the frame exceeds the configured limit.
pub fn encode_server_message(
    msg: &ServerMessage,
    options: Option<FrameDecoderOptions>,
) -> Result<Vec<u8>, CodecError> {
    let opts = CborOptions::from_frame(options);
    let value = msg
        .serialize(CborValueSerializer)
        .map_err(|e| CodecError::InvalidServer(e.to_string()))?;
    let payload = encode_cbor_value(&value, &opts)?;
    let frame = encode_frame(&payload);
    assert_complete_frame(&frame, options)?;
    Ok(frame)
}

/// Decodes a framed byte buffer into a [`ClientMessage`].
///
/// # Errors
///
/// Returns [`CodecError::Frame`] if the frame is malformed,
/// [`CodecError::Cbor`] if CBOR decoding fails,
/// [`CodecError::UnknownDiscriminant`] if the message type is unrecognized, or
/// [`CodecError::InvalidClient`] if the message fails validation.
pub fn decode_client_message(
    frame: &[u8],
    options: Option<FrameDecoderOptions>,
) -> Result<ClientMessage, CodecError> {
    let opts = CborOptions::from_frame(options);
    assert_complete_frame(frame, options)?;
    let value = decode_cbor_value(&frame[4..], &opts)?;
    check_client_discriminant(&value)?;
    let msg = ClientMessage::deserialize(CborValueDeserializer { value }).map_err(|e| {
        if e.0.contains("unknown variant") {
            CodecError::UnknownDiscriminant(e.0)
        } else {
            CodecError::InvalidClient(e.0)
        }
    })?;
    validate_client_message(&msg)?;
    Ok(msg)
}

/// Decodes a framed byte buffer into a [`ServerMessage`].
///
/// # Errors
///
/// Returns [`CodecError::Frame`] if the frame is malformed,
/// [`CodecError::Cbor`] if CBOR decoding fails,
/// [`CodecError::UnknownDiscriminant`] if the message type is unrecognized, or
/// [`CodecError::InvalidServer`] if the message fails validation.
pub fn decode_server_message(
    frame: &[u8],
    options: Option<FrameDecoderOptions>,
) -> Result<ServerMessage, CodecError> {
    let opts = CborOptions::from_frame(options);
    assert_complete_frame(frame, options)?;
    let value = decode_cbor_value(&frame[4..], &opts)?;
    check_server_discriminant(&value)?;
    let msg = ServerMessage::deserialize(CborValueDeserializer { value }).map_err(|e| {
        if e.0.contains("unknown variant") {
            CodecError::UnknownDiscriminant(e.0)
        } else {
            CodecError::InvalidServer(e.0)
        }
    })?;
    validate_server_message(&msg)?;
    Ok(msg)
}

/// Incrementally decodes framed client messages from arbitrary byte chunks.
pub struct ClientMessageDecoder {
    frames: FrameDecoder,
    options: Option<FrameDecoderOptions>,
    failed: bool,
}

impl ClientMessageDecoder {
    /// Creates a new decoder with the given frame options.
    ///
    /// # Errors
    ///
    /// Returns [`FrameError`] if the frame decoder cannot be initialized.
    pub fn new(options: Option<FrameDecoderOptions>) -> Result<Self, FrameError> {
        Ok(Self {
            frames: FrameDecoder::new(options)?,
            options,
            failed: false,
        })
    }
    /// Pushes a chunk of bytes and returns any complete messages.
    ///
    /// # Errors
    ///
    /// Returns [`CodecError`] if frame decoding, CBOR decoding, or message
    /// validation fails. Once an error occurs the decoder is permanently
    /// failed and subsequent calls return immediately.
    pub fn push(&mut self, chunk: &[u8]) -> Result<Vec<ClientMessage>, CodecError> {
        if self.failed {
            return Err(CodecError::InvalidClient(
                "client message decoder has failed".into(),
            ));
        }
        let payloads = self.frames.push(chunk)?;
        let opts = CborOptions::from_frame(self.options);
        let mut msgs = Vec::with_capacity(payloads.len());
        for p in payloads {
            let value = decode_cbor_value(&p, &opts)?;
            check_client_discriminant(&value)?;
            let msg = ClientMessage::deserialize(CborValueDeserializer { value }).map_err(|e| {
                self.failed = true;
                if e.0.contains("unknown variant") {
                    CodecError::UnknownDiscriminant(e.0)
                } else {
                    CodecError::InvalidClient(e.0)
                }
            })?;
            validate_client_message(&msg)?;
            msgs.push(msg);
        }
        Ok(msgs)
    }
    /// Signals end of input; returns an error if data remains buffered.
    ///
    /// # Errors
    ///
    /// Returns [`CodecError`] if the decoder has failed or if buffered
    /// data remains incomplete.
    pub fn end(&mut self) -> Result<(), CodecError> {
        if self.failed {
            return Err(CodecError::InvalidClient(
                "client message decoder has failed".into(),
            ));
        }
        self.frames.end()?;
        Ok(())
    }
}

/// Incrementally decodes framed server messages from arbitrary byte chunks.
pub struct ServerMessageDecoder {
    frames: FrameDecoder,
    options: Option<FrameDecoderOptions>,
    failed: bool,
}

impl ServerMessageDecoder {
    /// Creates a new decoder with the given frame options.
    ///
    /// # Errors
    ///
    /// Returns [`FrameError`] if the frame decoder cannot be initialized.
    pub fn new(options: Option<FrameDecoderOptions>) -> Result<Self, FrameError> {
        Ok(Self {
            frames: FrameDecoder::new(options)?,
            options,
            failed: false,
        })
    }
    /// Pushes a chunk of bytes and returns any complete messages.
    ///
    /// # Errors
    ///
    /// Returns [`CodecError`] if frame decoding, CBOR decoding, or message
    /// validation fails. Once an error occurs the decoder is permanently
    /// failed and subsequent calls return immediately.
    pub fn push(&mut self, chunk: &[u8]) -> Result<Vec<ServerMessage>, CodecError> {
        if self.failed {
            return Err(CodecError::InvalidServer(
                "server message decoder has failed".into(),
            ));
        }
        let payloads = self.frames.push(chunk)?;
        let opts = CborOptions::from_frame(self.options);
        let mut msgs = Vec::with_capacity(payloads.len());
        for p in payloads {
            let value = decode_cbor_value(&p, &opts)?;
            check_server_discriminant(&value)?;
            let msg = ServerMessage::deserialize(CborValueDeserializer { value }).map_err(|e| {
                self.failed = true;
                if e.0.contains("unknown variant") {
                    CodecError::UnknownDiscriminant(e.0)
                } else {
                    CodecError::InvalidServer(e.0)
                }
            })?;
            validate_server_message(&msg)?;
            msgs.push(msg);
        }
        Ok(msgs)
    }
    /// Signals end of input; returns an error if data remains buffered.
    ///
    /// # Errors
    ///
    /// Returns [`CodecError`] if the decoder has failed or if buffered
    /// data remains incomplete.
    pub fn end(&mut self) -> Result<(), CodecError> {
        if self.failed {
            return Err(CodecError::InvalidServer(
                "server message decoder has failed".into(),
            ));
        }
        self.frames.end()?;
        Ok(())
    }
}

/// Creates a [`ClientMessageDecoder`] with the given frame options.
///
/// # Errors
///
/// Returns [`FrameError`] if the frame decoder cannot be initialized.
pub fn create_client_message_decoder(
    options: Option<FrameDecoderOptions>,
) -> Result<ClientMessageDecoder, FrameError> {
    ClientMessageDecoder::new(options)
}

/// Creates a [`ServerMessageDecoder`] with the given frame options.
///
/// # Errors
///
/// Returns [`FrameError`] if the frame decoder cannot be initialized.
pub fn create_server_message_decoder(
    options: Option<FrameDecoderOptions>,
) -> Result<ServerMessageDecoder, FrameError> {
    ServerMessageDecoder::new(options)
}
