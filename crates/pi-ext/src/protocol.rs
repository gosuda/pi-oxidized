//! Versioned extension-host / pi-tui protocol (UTF-8 JSONL frames).
//!
//! One frame is a single JSON object on one line, at most [`MAX_FRAME_BYTES`]
//! UTF-8 bytes (excluding the trailing newline):
//!
//! ```json
//! {"id":1,"kind":"req","method":"hello","payload":{"protocolVersion":1,"compatibilityVersion":"0.80.10"}}
//! ```
//!
//! Rust is the authoritative validation boundary. TypeScript mirrors live in
//! `packages/pi-tui-protocol` and share golden JSONL fixtures under that
//! package's tests.

use std::collections::BTreeMap;
use std::fmt;
use std::str;

use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};
use thiserror::Error;
/// Open host-control method that synchronizes validated extension flag values.
pub const FLAGS_SET_METHOD: &str = "flags.set";

/// Open host-control method that dispatches one effective extension shortcut.
pub const SHORTCUT_EXECUTE_METHOD: &str = "shortcut.execute";

// Local wire copies of overlay layout value types (camelCase). Kept here so
// the protocol module does not depend on pi-tui compile health for validation.
// Field names match `pi_tui::layout::{SizeValue, OverlayAnchor, OverlayMargin, OverlaySpec}`.

/// Absolute cells or percent of a reference dimension.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SizeValue {
    /// Absolute size in terminal cells.
    Cells(u16),
    /// Percentage of the reference size (`0..=100`).
    Percent(u8),
}

impl Serialize for SizeValue {
    fn serialize<S: serde::Serializer>(
        &self,
        serializer: S,
    ) -> std::result::Result<S::Ok, S::Error> {
        match *self {
            Self::Cells(n) => serializer.serialize_u16(n),
            Self::Percent(n) => serializer.serialize_str(&format!("{n}%")),
        }
    }
}

impl<'de> Deserialize<'de> for SizeValue {
    fn deserialize<D: serde::Deserializer<'de>>(
        deserializer: D,
    ) -> std::result::Result<Self, D::Error> {
        struct Visitor;
        impl serde::de::Visitor<'_> for Visitor {
            type Value = SizeValue;
            fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.write_str("a cell count number or a percent string like \"50%\"")
            }
            fn visit_u64<E: serde::de::Error>(self, v: u64) -> std::result::Result<Self::Value, E> {
                u16::try_from(v)
                    .map(SizeValue::Cells)
                    .map_err(|_| E::custom("size value exceeds u16"))
            }
            fn visit_i64<E: serde::de::Error>(self, v: i64) -> std::result::Result<Self::Value, E> {
                if v < 0 {
                    return Err(E::custom("size value must be non-negative"));
                }
                self.visit_u64(v.cast_unsigned())
            }
            fn visit_str<E: serde::de::Error>(
                self,
                v: &str,
            ) -> std::result::Result<Self::Value, E> {
                let stripped = v
                    .strip_suffix('%')
                    .ok_or_else(|| E::custom(format!("invalid percent size: {v}")))?;
                if stripped.is_empty() || !stripped.bytes().all(|b| b.is_ascii_digit()) {
                    return Err(E::custom(format!("invalid percent size: {v}")));
                }
                let n: u32 = stripped.parse().map_err(E::custom)?;
                Ok(SizeValue::Percent(
                    u8::try_from(n.min(100)).map_err(E::custom)?,
                ))
            }
        }
        deserializer.deserialize_any(Visitor)
    }
}

/// Anchor point for overlay placement.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum OverlayAnchor {
    /// Center of the available area.
    #[default]
    Center,
    /// Top-left corner.
    TopLeft,
    /// Top-right corner.
    TopRight,
    /// Bottom-left corner.
    BottomLeft,
    /// Bottom-right corner.
    BottomRight,
    /// Top edge, horizontally centered.
    TopCenter,
    /// Bottom edge, horizontally centered.
    BottomCenter,
    /// Left edge, vertically centered.
    LeftCenter,
    /// Right edge, vertically centered.
    RightCenter,
}

/// Per-side overlay margin from terminal edges.
///
/// This is the direct `pi_ext::protocol` copy. Unlike
/// [`OverlayMarginWire`], it performs the canonical one-way normalization
/// shared with `pi_tui::layout::OverlayMargin`: a bare scalar such as `4`
/// deserializes to a uniform four-side value, and the value always serializes
/// as the normalized `{ top, right, bottom, left }` object. `OverlaySpec`
/// keeps [`OverlayMarginWire`] so the portable scalar round-trip stays part of
/// the shared Rust/TypeScript fixture contract; do not replace it there.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct OverlayMargin {
    /// Top margin in rows.
    pub top: u16,
    /// Right margin in columns.
    pub right: u16,
    /// Bottom margin in rows.
    pub bottom: u16,
    /// Left margin in columns.
    pub left: u16,
}

impl OverlayMargin {
    /// Same margin on every side.
    #[must_use]
    pub const fn uniform(n: u16) -> Self {
        Self {
            top: n,
            right: n,
            bottom: n,
            left: n,
        }
    }
}

impl<'de> Deserialize<'de> for OverlayMargin {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        struct Visitor;

        impl<'de> serde::de::Visitor<'de> for Visitor {
            type Value = OverlayMargin;

            fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.write_str(
                    "a margin number or an object with optional top/right/bottom/left sides",
                )
            }

            fn visit_u64<E: serde::de::Error>(self, v: u64) -> Result<Self::Value, E> {
                u16::try_from(v)
                    .map(OverlayMargin::uniform)
                    .map_err(|_| E::custom("margin value exceeds u16"))
            }

            fn visit_i64<E: serde::de::Error>(self, v: i64) -> Result<Self::Value, E> {
                if v < 0 {
                    return Err(E::custom("margin must be non-negative"));
                }
                self.visit_u64(v.cast_unsigned())
            }

            fn visit_map<A: serde::de::MapAccess<'de>>(
                self,
                mut map: A,
            ) -> Result<Self::Value, A::Error> {
                let mut top = 0u16;
                let mut right = 0u16;
                let mut bottom = 0u16;
                let mut left = 0u16;
                while let Some(key) = map.next_key::<String>()? {
                    match key.as_str() {
                        "top" => top = map.next_value()?,
                        "right" => right = map.next_value()?,
                        "bottom" => bottom = map.next_value()?,
                        "left" => left = map.next_value()?,
                        _ => {
                            let _: serde::de::IgnoredAny = map.next_value()?;
                        }
                    }
                }
                Ok(OverlayMargin {
                    top,
                    right,
                    bottom,
                    left,
                })
            }
        }

        deserializer.deserialize_any(Visitor)
    }
}
/// Wire margin accepts either a uniform scalar or per-side object.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum OverlayMarginWire {
    /// Uniform margin applied to all four sides.
    Uniform(u16),
    /// Individually specified sides.
    Sides(OverlayMargin),
}

/// Serializable overlay layout specification for `uiSlot.overlayOptions`.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct OverlaySpec {
    /// Width in columns, or percentage of terminal width.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub width: Option<SizeValue>,
    /// Minimum width in columns.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub min_width: Option<u16>,
    /// Maximum height in rows, or percentage of terminal height.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub max_height: Option<SizeValue>,
    /// Anchor point when `row`/`col` are unset (default center).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub anchor: Option<OverlayAnchor>,
    /// Horizontal offset from the resolved position (positive = right).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub offset_x: Option<i16>,
    /// Vertical offset from the resolved position (positive = down).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub offset_y: Option<i16>,
    /// Absolute or percent row position (overrides vertical anchor).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub row: Option<SizeValue>,
    /// Absolute or percent column position (overrides horizontal anchor).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub col: Option<SizeValue>,
    /// Margin from terminal edges: a uniform scalar or per-side object.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub margin: Option<OverlayMarginWire>,
    /// When true, showing the overlay does not capture keyboard focus.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub non_capturing: bool,
}

/// Wire protocol version negotiated in [`Hello`] / [`HelloAck`].
pub const PROTOCOL_VERSION: u32 = 1;

/// Compatibility target: reference `@earendil-works/pi-coding-agent` version.
pub const COMPATIBILITY_VERSION: &str = "0.80.10";

/// Maximum UTF-8 byte length of one frame line (excluding the trailing newline).
pub const MAX_FRAME_BYTES: usize = 8 * 1024 * 1024;

/// Correlation identifier for request/response/error frames.
///
/// Event frames use `0` when unsolicited, or the parent request id when
/// streaming updates for that call.
pub type FrameId = u64;

/// Result type for protocol encode/decode/validation operations.
pub type Result<T, E = ProtocolError> = std::result::Result<T, E>;
/// Protocol encode, decode, validation, or handshake failure.
#[derive(Debug, Error, Clone, PartialEq, Eq)]
pub enum ProtocolError {
    /// Frame line exceeded [`MAX_FRAME_BYTES`] before a newline arrived.
    #[error("frame exceeds maximum size of {MAX_FRAME_BYTES} bytes")]
    FrameTooLarge,
    /// Bytes were not valid UTF-8.
    #[error("invalid UTF-8 in protocol stream: {0}")]
    InvalidUtf8(String),
    /// A complete line was not valid JSON.
    #[error("invalid JSON frame: {0}")]
    InvalidJson(String),
    /// JSON decoded but was not a protocol frame object.
    #[error("malformed frame: {0}")]
    MalformedFrame(String),
    /// Frame kind/id/method rules were violated.
    #[error("invalid frame: {0}")]
    InvalidFrame(String),
    /// Hello handshake versions are incompatible.
    #[error("protocol version mismatch: remote={remote} local={local}")]
    VersionMismatch {
        /// Remote peer protocol version.
        remote: u32,
        /// Local protocol version.
        local: u32,
    },
    /// Compatibility string does not match the supported coding-agent version.
    #[error("compatibility version mismatch: remote={remote} local={local}")]
    CompatibilityMismatch {
        /// Remote compatibility version string.
        remote: String,
        /// Local compatibility version string.
        local: String,
    },
    /// Method name is not in the bridge/host-control allowlist.
    #[error("unknown protocol method: {0}")]
    UnknownMethod(String),
    /// Stream ended while a partial line was still buffered.
    #[error("truncated protocol frame at end of stream")]
    Truncated,
}

/// Frame kind discriminant on the wire (`req` | `res` | `event` | `error`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum FrameKind {
    /// Correlated request; requires nonzero [`Frame::id`].
    Req,
    /// Correlated success response; requires nonzero [`Frame::id`].
    Res,
    /// Unsolicited or streaming event; id may be `0` or a parent request id.
    #[default]
    Event,
    /// Correlated or unsolicited error; nonzero id when correlated.
    Error,
}

impl FrameKind {
    /// Wire string for this kind.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Req => "req",
            Self::Res => "res",
            Self::Event => "event",
            Self::Error => "error",
        }
    }

    /// Parse a wire kind string.
    #[must_use]
    pub fn parse(raw: &str) -> Option<Self> {
        match raw {
            "req" => Some(Self::Req),
            "res" => Some(Self::Res),
            "event" => Some(Self::Event),
            "error" => Some(Self::Error),
            _ => None,
        }
    }

    /// Whether this kind requires a nonzero frame id.
    #[must_use]
    pub const fn requires_nonzero_id(self) -> bool {
        matches!(self, Self::Req | Self::Res)
    }
}

impl fmt::Display for FrameKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Allowlisted bridge and host-control methods.
///
/// Lifecycle event methods reuse the exact `type` discriminants from the
/// reference extension API and are carried as open method strings on
/// [`Frame`]; this enum covers the fixed bridge surface plus host-control
/// dialog / input / slot methods that have typed payloads in this module.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum Method {
    /// Protocol handshake request/response.
    Hello,
    /// Extension tool partial UI/result update.
    ToolUpdate,
    /// Extension custom-provider stream event.
    ProviderEvent,
    /// Host-rendered UI slot push (structured runs).
    UiSlot,
    /// Dispose a previously pushed UI slot.
    DisposeSlot,
    /// Non-retryable extension failure notification.
    ExtensionError,
    /// Native select dialog.
    Select,
    /// Native confirm dialog.
    Confirm,
    /// Native single-line input dialog.
    Input,
    /// Native multi-line editor dialog.
    Editor,
    /// Fire-and-forget notification.
    Notify,
    /// Raw terminal input to a registered host handler / focused slot.
    TerminalInput,
    /// Structured UI event delivered to a focused host component.
    UiEvent,
    /// Request a host component measure for a given width/theme generation.
    Measure,
    /// Request a host component render for a given width/theme generation.
    Render,
}

impl Method {
    /// All allowlisted methods in stable order.
    pub const ALL: &'static [Self] = &[
        Self::Hello,
        Self::ToolUpdate,
        Self::ProviderEvent,
        Self::UiSlot,
        Self::DisposeSlot,
        Self::ExtensionError,
        Self::Select,
        Self::Confirm,
        Self::Input,
        Self::Editor,
        Self::Notify,
        Self::TerminalInput,
        Self::UiEvent,
        Self::Measure,
        Self::Render,
    ];

    /// Wire method string.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Hello => "hello",
            Self::ToolUpdate => "toolUpdate",
            Self::ProviderEvent => "providerEvent",
            Self::UiSlot => "uiSlot",
            Self::DisposeSlot => "disposeSlot",
            Self::ExtensionError => "extensionError",
            Self::Select => "select",
            Self::Confirm => "confirm",
            Self::Input => "input",
            Self::Editor => "editor",
            Self::Notify => "notify",
            Self::TerminalInput => "terminalInput",
            Self::UiEvent => "uiEvent",
            Self::Measure => "measure",
            Self::Render => "render",
        }
    }

    /// Parse an allowlisted method string.
    #[must_use]
    pub fn parse(raw: &str) -> Option<Self> {
        match raw {
            "hello" => Some(Self::Hello),
            "toolUpdate" => Some(Self::ToolUpdate),
            "providerEvent" => Some(Self::ProviderEvent),
            "uiSlot" => Some(Self::UiSlot),
            "disposeSlot" => Some(Self::DisposeSlot),
            "extensionError" => Some(Self::ExtensionError),
            "select" => Some(Self::Select),
            "confirm" => Some(Self::Confirm),
            "input" => Some(Self::Input),
            "editor" => Some(Self::Editor),
            "notify" => Some(Self::Notify),
            "terminalInput" => Some(Self::TerminalInput),
            "uiEvent" => Some(Self::UiEvent),
            "measure" => Some(Self::Measure),
            "render" => Some(Self::Render),
            _ => None,
        }
    }

    /// Whether `raw` is an allowlisted bridge/host-control method.
    #[must_use]
    pub fn is_allowlisted(raw: &str) -> bool {
        Self::parse(raw).is_some()
    }
}

impl fmt::Display for Method {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}
/// Wire name for [`Method::UiSlot`].
#[must_use]
pub const fn ui_slot_method() -> &'static str {
    "uiSlot"
}

/// Wire name for [`Method::DisposeSlot`].
#[must_use]
pub const fn dispose_slot_method() -> &'static str {
    "disposeSlot"
}

/// Wire name for [`Method::ToolUpdate`].
#[must_use]
pub const fn tool_update_method() -> &'static str {
    "toolUpdate"
}

/// Wire name for [`Method::ProviderEvent`].
#[must_use]
pub const fn provider_event_method() -> &'static str {
    "providerEvent"
}

/// Wire name for [`Method::ExtensionError`].
#[must_use]
pub const fn extension_error_method() -> &'static str {
    "extensionError"
}

/// Compatibility validation message helper used by the host client.
pub struct FrameValidationError;

impl FrameValidationError {
    /// Generic validation message for a rejected frame.
    #[must_use]
    pub const fn message_for(_frame: &Frame) -> &'static str {
        "invalid protocol frame"
    }
}

/// One protocol frame.
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Frame {
    /// Correlation id (`0` only for unsolicited events / uncorrelated errors).
    pub id: FrameId,
    /// Frame kind.
    pub kind: FrameKind,
    /// Method name (bridge, host-control, or lifecycle event type).
    pub method: String,
    /// Method-specific JSON payload (object for typed methods).
    #[serde(default)]
    pub payload: Value,
}

impl Frame {
    /// Build a frame with a typed method.
    #[must_use]
    pub fn new(id: FrameId, kind: FrameKind, method: Method, payload: Value) -> Self {
        Self {
            id,
            kind,
            method: method.as_str().to_owned(),
            payload,
        }
    }

    /// Build a request frame.
    #[must_use]
    pub fn request(id: FrameId, method: Method, payload: Value) -> Self {
        Self::new(id, FrameKind::Req, method, payload)
    }

    /// Build a response frame.
    #[must_use]
    pub fn response(id: FrameId, method: Method, payload: Value) -> Self {
        Self::new(id, FrameKind::Res, method, payload)
    }

    /// Build an event frame.
    #[must_use]
    pub fn event(id: FrameId, method: Method, payload: Value) -> Self {
        Self::new(id, FrameKind::Event, method, payload)
    }

    /// Build an error frame with a structured error body.
    ///
    /// # Errors
    ///
    /// Returns [`ProtocolError::InvalidJson`] if the error body cannot be
    /// serialized (should not occur for well-formed [`ErrorPayload`] values).
    pub fn error_frame(id: FrameId, method: Method, error: &ErrorPayload) -> Result<Self> {
        let payload = serde_json::to_value(error)
            .map_err(|e| ProtocolError::InvalidJson(format!("serialize error payload: {e}")))?;
        Ok(Self::new(id, FrameKind::Error, method, payload))
    }

    /// Parse the method field as an allowlisted [`Method`].
    #[must_use]
    pub fn method_enum(&self) -> Option<Method> {
        Method::parse(&self.method)
    }

    /// Validate id/kind rules and, when `require_allowlisted`, the method set.
    ///
    /// # Errors
    ///
    /// Returns [`ProtocolError::InvalidFrame`] or
    /// [`ProtocolError::UnknownMethod`] when validation fails.
    pub fn validate(&self, require_allowlisted: bool) -> Result<()> {
        if self.kind.requires_nonzero_id() && self.id == 0 {
            return Err(ProtocolError::InvalidFrame(format!(
                "kind {} requires nonzero id",
                self.kind
            )));
        }
        if self.method.is_empty() {
            return Err(ProtocolError::InvalidFrame(
                "method must be a non-empty string".to_owned(),
            ));
        }
        if require_allowlisted && !Method::is_allowlisted(&self.method) {
            return Err(ProtocolError::UnknownMethod(self.method.clone()));
        }
        // Reject scalar payloads; typed methods use objects (arrays allowed for open payloads).
        match &self.payload {
            Value::Null | Value::Object(_) | Value::Array(_) => {}
            Value::Bool(_) | Value::Number(_) | Value::String(_) => {
                return Err(ProtocolError::InvalidFrame(
                    "payload must be a JSON object or array".to_owned(),
                ));
            }
        }
        if self.method == Method::UiSlot.as_str() {
            let slot: UiSlot = serde_json::from_value(self.payload.clone()).map_err(|error| {
                ProtocolError::InvalidFrame(format!("invalid uiSlot payload: {error}"))
            })?;
            slot.validate()?;
        }
        Ok(())
    }
}

/// Client → host hello request payload.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Hello {
    /// Protocol version (`1`).
    pub protocol_version: u32,
    /// Reference coding-agent compatibility version.
    pub compatibility_version: String,
}

impl Hello {
    /// Local hello payload for the current build.
    #[must_use]
    pub fn local() -> Self {
        Self {
            protocol_version: PROTOCOL_VERSION,
            compatibility_version: COMPATIBILITY_VERSION.to_owned(),
        }
    }

    /// Validate remote hello against local constants.
    ///
    /// # Errors
    ///
    /// Returns version or compatibility mismatch errors.
    pub fn validate_remote(&self) -> Result<()> {
        if self.protocol_version != PROTOCOL_VERSION {
            return Err(ProtocolError::VersionMismatch {
                remote: self.protocol_version,
                local: PROTOCOL_VERSION,
            });
        }
        if self.compatibility_version != COMPATIBILITY_VERSION {
            return Err(ProtocolError::CompatibilityMismatch {
                remote: self.compatibility_version.clone(),
                local: COMPATIBILITY_VERSION.to_owned(),
            });
        }
        Ok(())
    }
}

/// Host → client hello acknowledgment payload.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct HelloAck {
    /// Protocol version accepted by the peer.
    pub protocol_version: u32,
    /// Compatibility version accepted by the peer.
    pub compatibility_version: String,
}

impl HelloAck {
    /// Local acknowledgment payload.
    #[must_use]
    pub fn local() -> Self {
        Self {
            protocol_version: PROTOCOL_VERSION,
            compatibility_version: COMPATIBILITY_VERSION.to_owned(),
        }
    }
}

/// Structured error payload for `kind: "error"` frames.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ErrorPayload {
    /// Stable machine-readable error code.
    pub code: String,
    /// Human-readable message.
    pub message: String,
    /// Whether the caller may retry the same side effect (always false for
    /// extension failures per host policy).
    pub retryable: bool,
    /// Optional structured detail.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub data: Option<Value>,
}

impl ErrorPayload {
    /// Non-retryable error without detail data.
    #[must_use]
    pub fn new(code: impl Into<String>, message: impl Into<String>) -> Self {
        Self {
            code: code.into(),
            message: message.into(),
            retryable: false,
            data: None,
        }
    }
}

/// Allowlisted text style for structured UI runs (no raw ANSI).
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct Style {
    /// Bold emphasis.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub bold: Option<bool>,
    /// Dim/faint emphasis.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub dim: Option<bool>,
    /// Italic emphasis.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub italic: Option<bool>,
    /// Underline.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub underline: Option<bool>,
    /// Reverse video.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reverse: Option<bool>,
    /// Strikethrough.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub strikethrough: Option<bool>,
    /// Foreground color.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fg: Option<WireColor>,
    /// Background color.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub bg: Option<WireColor>,
    /// Optional validated hyperlink (OSC 8 fields).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub link: Option<Hyperlink>,
}

/// Allowlisted color encoding for styled runs.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "camelCase")]
pub enum WireColor {
    /// Named 8/16-color palette entry.
    Named {
        /// Palette name (`black`, `red`, …, `brightWhite`).
        name: NamedColor,
    },
    /// 256-color index.
    Indexed {
        /// Palette index `0..=255`.
        index: u8,
    },
    /// Truecolor RGB triple.
    Rgb {
        /// Red channel.
        r: u8,
        /// Green channel.
        g: u8,
        /// Blue channel.
        b: u8,
    },
}

/// Named ANSI palette colors (standard + bright).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum NamedColor {
    /// Black.
    Black,
    /// Red.
    Red,
    /// Green.
    Green,
    /// Yellow.
    Yellow,
    /// Blue.
    Blue,
    /// Magenta.
    Magenta,
    /// Cyan.
    Cyan,
    /// White.
    White,
    /// Bright black / gray.
    BrightBlack,
    /// Bright red.
    BrightRed,
    /// Bright green.
    BrightGreen,
    /// Bright yellow.
    BrightYellow,
    /// Bright blue.
    BrightBlue,
    /// Bright magenta.
    BrightMagenta,
    /// Bright cyan.
    BrightCyan,
    /// Bright white.
    BrightWhite,
}

/// Validated OSC 8 hyperlink fields (http/https only on the Rust boundary).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Hyperlink {
    /// Optional link id (≤ 128 bytes when validated).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub id: Option<String>,
    /// Absolute URI (`http` / `https`, ≤ 2048 bytes when validated).
    pub uri: String,
}

impl Hyperlink {
    /// Maximum accepted id length in bytes.
    pub const MAX_ID_BYTES: usize = 128;
    /// Maximum accepted URI length in bytes.
    pub const MAX_URI_BYTES: usize = 2048;

    /// Validate scheme and size limits.
    ///
    /// # Errors
    ///
    /// Returns [`ProtocolError::InvalidFrame`] when the link is rejected.
    pub fn validate(&self) -> Result<()> {
        if let Some(id) = &self.id
            && id.len() > Self::MAX_ID_BYTES
        {
            return Err(ProtocolError::InvalidFrame(format!(
                "hyperlink id exceeds {} bytes",
                Self::MAX_ID_BYTES
            )));
        }
        if self.uri.len() > Self::MAX_URI_BYTES {
            return Err(ProtocolError::InvalidFrame(format!(
                "hyperlink uri exceeds {} bytes",
                Self::MAX_URI_BYTES
            )));
        }
        let ok = self.uri.starts_with("http://") || self.uri.starts_with("https://");
        if !ok {
            return Err(ProtocolError::InvalidFrame(
                "hyperlink uri must use http or https".to_owned(),
            ));
        }
        Ok(())
    }
}
impl UiSlot {
    /// Validate every hyperlink carried by every styled run.
    ///
    /// # Errors
    ///
    /// Returns [`ProtocolError::InvalidFrame`] for a forbidden scheme or an
    /// oversized hyperlink id/URI.
    pub fn validate(&self) -> Result<()> {
        for line in &self.runs {
            for run in line {
                if let Some(link) = &run.style.link {
                    link.validate()?;
                }
            }
        }
        Ok(())
    }
}

/// One contiguous styled text run inside a UI slot line.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct StyledRun {
    /// Printable text (no embedded newlines; tabs expanded by the host).
    pub text: String,
    /// Optional style; omitted/default means unstyled.
    #[serde(default, skip_serializing_if = "is_default_style")]
    pub style: Style,
}

fn is_default_style(style: &Style) -> bool {
    style == &Style::default()
}

/// Where a host UI slot is placed in the native composition tree.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum SlotPlacement {
    /// Startup / chat header region.
    #[default]
    Header,
    /// Footer region.
    Footer,
    /// Widget row above the editor.
    AboveEditor,
    /// Widget row below the editor.
    BelowEditor,
    /// Full editor replacement.
    Editor,
    /// Custom message / entry renderer.
    MessageRenderer,
    /// Modal overlay.
    Overlay,
}

/// Cursor cell within a focusable slot (column/row in the slot's content).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SlotCursor {
    /// Zero-based column.
    pub col: u16,
    /// Zero-based row.
    pub row: u16,
}

/// Host → Rust `uiSlot` event payload (structured runs only).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct UiSlot {
    /// Stable slot key.
    pub key: String,
    /// Monotonic generation; stale generations are discarded.
    pub generation: u64,
    /// Composition placement.
    pub placement: SlotPlacement,
    /// Measured height in rows.
    pub height: u16,
    /// Lines of styled runs (`lines[row][run]`).
    pub runs: Vec<Vec<StyledRun>>,
    /// Whether the slot can receive focus / input.
    #[serde(default)]
    pub focusable: bool,
    /// Optional hardware-cursor hint inside the slot.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cursor: Option<SlotCursor>,
    /// Overlay layout options when [`SlotPlacement::Overlay`].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub overlay_options: Option<OverlaySpec>,
}

/// Dispose a keyed slot (and any focused state).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DisposeSlot {
    /// Slot key to dispose.
    pub key: String,
    /// Optional generation that triggered dispose.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub generation: Option<u64>,
}

/// Non-retryable extension failure event payload.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ExtensionErrorEvent {
    /// Stable error code.
    pub code: String,
    /// Human-readable message.
    pub message: String,
    /// Always false for extension side effects.
    #[serde(default)]
    pub retryable: bool,
    /// Optional extension path / detail.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub data: Option<Value>,
}

/// Partial tool update from the host.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ToolUpdate {
    /// Tool call id.
    pub tool_call_id: String,
    /// Tool name.
    pub tool_name: String,
    /// Partial result payload (open JSON).
    pub partial_result: Value,
}

/// Custom provider stream event from the host.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ProviderEvent {
    /// Provider registration id.
    pub provider_id: String,
    /// Stream / call correlation id.
    pub call_id: String,
    /// Event name within the provider stream.
    pub event: String,
    /// Event payload (open JSON).
    #[serde(default)]
    pub data: Value,
}

/// Open method string: the host emits this event after a committed live
/// provider mutation (register/unregister from a command or delayed callback).
/// It is an open dotted control name, not a [`Method`] enum variant, so strict
/// method decoding rejects it while the extension bridge accepts it.
pub const PROVIDERS_UPDATE_METHOD: &str = "providers.update";

/// One provider entry in a [`ProvidersUpdate`] frame.
///
/// Mirrors the host's provider registration shape (camelCase). The Rust bridge
/// converts each entry into a `ProviderConfigInput`; unknown fields are ignored.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct ProviderUpdateEntry {
    /// Provider display name / id.
    pub name: String,
    /// Base URL for the provider API.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub base_url: Option<String>,
    /// API kind (e.g. `openai-completions`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub api: Option<String>,
    /// Optional API key.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub api_key: Option<String>,
    /// Optional static headers.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub headers: Option<BTreeMap<String, String>>,
    /// Whether the host sends the auth header for this provider.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub auth_header: Option<bool>,
    /// Models declared by the provider.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub models: Option<Vec<Value>>,
    /// `true` when the host holds a live `streamSimple` function.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub stream_simple: bool,
    /// Optional extension path used in diagnostics.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub extension_path: Option<String>,
}

/// `providers.update` event payload (host → Rust). Carries the sender
/// endpoint's complete current provider registry after a committed live
/// mutation; the Rust bridge replaces only that endpoint's provider snapshot.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct ProvidersUpdate {
    /// The endpoint's complete current provider list.
    #[serde(default)]
    pub providers: Vec<ProviderUpdateEntry>,
}

/// Key modifiers on the wire (shift|alt|ctrl|super).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct KeyModifiersWire {
    /// Shift.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub shift: Option<bool>,
    /// Alt / option.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub alt: Option<bool>,
    /// Control.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ctrl: Option<bool>,
    /// Super / meta / command.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub super_key: Option<bool>,
}

/// Key event kind on the wire.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum KeyEventKindWire {
    /// Key press (default).
    #[default]
    Press,
    /// Key release (Kitty).
    Release,
    /// Key repeat.
    Repeat,
}

/// Structured UI event delivered over the protocol (never Ratatui/crossterm types).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "camelCase")]
pub enum UiEventWire {
    /// Keyboard event.
    Key {
        /// Key id grammar base (`enter`, `a`, `f1`, …).
        code: String,
        /// Modifier set.
        #[serde(default)]
        modifiers: KeyModifiersWire,
        /// Press / release / repeat.
        #[serde(default)]
        kind: KeyEventKindWire,
    },
    /// Bracketed paste text (newlines normalized to `\n`).
    Paste {
        /// Pasted text.
        text: String,
    },
    /// Terminal focus gained.
    FocusGained,
    /// Terminal focus lost.
    FocusLost,
    /// Terminal resize.
    Resize {
        /// Columns.
        width: u16,
        /// Rows.
        height: u16,
    },
}

/// Validated extension flag value sent to the TypeScript runtime.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum FlagValueWire {
    /// Boolean CLI flag.
    Boolean(bool),
    /// String CLI flag.
    String(String),
}

/// Payload for [`FLAGS_SET_METHOD`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct FlagsSetRequest {
    /// Complete validated flag-value overlay.
    pub values: BTreeMap<String, FlagValueWire>,
}

/// Acknowledgement for [`FLAGS_SET_METHOD`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct FlagsSetResponse {
    /// True when the host applied every supplied value.
    pub ok: bool,
}

/// Payload for [`SHORTCUT_EXECUTE_METHOD`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ShortcutExecuteRequest {
    /// Canonical lower-case key identifier.
    pub key: String,
}

/// Immediate dispatch acknowledgement for [`SHORTCUT_EXECUTE_METHOD`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ShortcutExecuteResponse {
    /// Whether a live extension shortcut owned this key.
    pub handled: bool,
}

/// Keyed UI event request for [`Method::UiEvent`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct UiEventRequest {
    /// UI slot key.
    pub key: String,
    /// Slot generation observed by the native product.
    pub generation: u64,
    /// Structured event for cross-language inspection.
    pub event: UiEventWire,
    /// Raw terminal input bytes for component `handleInput`, when applicable.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub data: Option<String>,
}

/// Host delivery result for [`Method::UiEvent`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct UiEventResponse {
    /// True only when the key and generation matched a live component.
    pub delivered: bool,
}

/// Terminal-input rewrite / consume result for `terminalInput`.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct TerminalInputResult {
    /// When true, native handling is skipped.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub consume: bool,
    /// Optional rewritten input data.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub data: Option<String>,
}

/// Dialog timeout option shared by select/confirm/input.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct DialogOptions {
    /// Auto-dismiss timeout in milliseconds.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub timeout_ms: Option<u64>,
}

/// `select` request payload.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SelectRequest {
    /// Dialog title.
    pub title: String,
    /// Options presented to the user.
    pub options: Vec<String>,
    /// Optional timeout.
    #[serde(default, flatten)]
    pub options_meta: DialogOptions,
}

/// `select` response payload.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SelectResponse {
    /// Chosen option, or `null`/missing when dismissed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub value: Option<String>,
}

/// `confirm` request payload.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ConfirmRequest {
    /// Dialog title.
    pub title: String,
    /// Dialog message body.
    pub message: String,
    /// Optional timeout.
    #[serde(default, flatten)]
    pub options_meta: DialogOptions,
}

/// `confirm` response payload.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ConfirmResponse {
    /// Whether the user confirmed.
    pub confirmed: bool,
}

/// `input` request payload.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct InputRequest {
    /// Dialog title.
    pub title: String,
    /// Placeholder text.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub placeholder: Option<String>,
    /// Optional timeout.
    #[serde(default, flatten)]
    pub options_meta: DialogOptions,
}

/// `input` response payload.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct InputResponse {
    /// Entered value, or dismissed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub value: Option<String>,
}

/// `editor` request payload.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct EditorRequest {
    /// Dialog title.
    pub title: String,
    /// Prefill text.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub prefill: Option<String>,
}

/// `editor` response payload.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct EditorResponse {
    /// Edited value, or dismissed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub value: Option<String>,
}

/// Notification level for `notify`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum NotifyLevel {
    /// Informational.
    #[default]
    Info,
    /// Warning.
    Warning,
    /// Error.
    Error,
}

/// `notify` request/event payload.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct NotifyRequest {
    /// Notification text.
    pub message: String,
    /// Severity.
    #[serde(default, rename = "type")]
    pub level: NotifyLevel,
}

/// Measure/render request shared fields.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SlotRenderRequest {
    /// Slot key.
    pub key: String,
    /// Available width in columns.
    pub width: u16,
    /// Theme generation counter.
    pub theme_generation: u64,
}

/// Measure response height.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct MeasureResponse {
    /// Measured height in rows.
    pub height: u16,
}
// ---------------------------------------------------------------------------
// Theme wire types (open methods, outside the fixed Method allowlist)
// ---------------------------------------------------------------------------

/// Open method string: Rust pushes the active theme + catalog to the host.
pub const THEME_UPDATE_METHOD: &str = "theme.update";

/// Open method string: the host applies an extension `setTheme` call.
pub const THEME_SET_METHOD: &str = "theme.set";

/// One theme slot value: `""` (reset), a 256-color index, or `"#rrggbb"`.
///
/// Mirrors the upstream theme-JSON `ColorValue` vocabulary so the host can
/// feed values straight into a reference-shaped `Theme` constructor.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum ThemeColorValue {
    /// 256-color palette index.
    Index(u8),
    /// `"#rrggbb"` hex, or `""` for reset.
    Text(String),
}

/// A fully resolved theme as extensions observe it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ThemeWire {
    /// Display name (`None` for in-memory themes).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    /// Source JSON path when file-backed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source_path: Option<String>,
    /// `"truecolor"` or `"256color"`.
    pub color_mode: String,
    /// Foreground slot values keyed by schema slot name.
    pub fg: BTreeMap<String, ThemeColorValue>,
    /// Background slot values keyed by schema slot name.
    pub bg: BTreeMap<String, ThemeColorValue>,
}

/// One catalog entry for `getAllThemes` / `getTheme`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ThemeCatalogEntry {
    /// Display name.
    pub name: String,
    /// Theme JSON path (built-ins report the shipped path).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub path: Option<String>,
    /// Custom-theme file stem when it differs from `name` (upstream
    /// `getTheme` loads customs by filename).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub file_stem: Option<String>,
    /// Fully resolved colors.
    pub theme: ThemeWire,
}

/// `theme.update` event payload (Rust → host).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ThemeUpdate {
    /// The active resolved theme.
    pub theme: ThemeWire,
    /// Detected terminal polarity: `"dark"` or `"light"`.
    pub terminal_theme: String,
    /// Active `themeMode`: `"auto"`, `"light"`, or `"dark"`.
    pub theme_mode: String,
    /// Runtime theme generation (flows back on measure/render).
    pub theme_generation: u64,
    /// Every available theme (built-ins + discovered customs).
    pub themes: Vec<ThemeCatalogEntry>,
}

/// `theme.set` event payload (host → Rust). Exactly one of `name` / `theme`
/// is set: the string form carries the raw name or `light/dark` pair; the
/// object form applies without persistence.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ThemeSet {
    /// Raw theme name or slash pair.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    /// In-memory theme instance.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub theme: Option<ThemeWire>,
    /// Whether Rust should persist the setting (string form on success).
    #[serde(default)]
    pub persist: bool,
}
// ---------------------------------------------------------------------------
// Session-action bridge wire types (open methods)
// ---------------------------------------------------------------------------

/// Open method string: Rust pushes the mirrored session state to the host.
///
/// The host serves the synchronous `ExtensionActions` / context getters
/// (`getSessionName`, `getActiveTools`, `isIdle`, …) from the latest push.
pub const SESSION_UPDATE_METHOD: &str = "session.update";

/// Open method string: the host forwards a fire-and-forget extension session
/// action (`pi.setSessionName`, `ctx.abort`, …).
pub const SESSION_COMMAND_METHOD: &str = "session.command";

/// Open method string: correlated `pi.setModel` request (host → Rust).
pub const SESSION_SET_MODEL_METHOD: &str = "session.setModel";

/// Open method string: the host forwards a fire-and-forget extension UI
/// control (`ui.setStatus`, `ui.setEditorText`, …).
pub const UI_CONTROL_METHOD: &str = "ui.control";

/// Open method string: Rust pushes mirrored UI state (editor text, tool
/// expansion) so the host can serve `getEditorText` / `getToolsExpanded`.
pub const UI_STATE_METHOD: &str = "ui.state";

/// One registered tool as extensions observe it via `pi.getAllTools()`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SessionToolWire {
    /// Registered tool name.
    pub name: String,
    /// Human-readable description.
    pub description: String,
    /// JSON Schema for arguments.
    pub parameters: Value,
    /// Origin label (`builtin` or `extension:<name>`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source: Option<String>,
}

/// One slash command as extensions observe it via `pi.getCommands()`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SessionCommandInfoWire {
    /// Command name without leading `/`.
    pub name: String,
    /// Optional description.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    /// Origin discriminant (`extension` | `prompt` | `skill`).
    pub source: String,
}

/// One model scoped to this session (`--models` / `enabledModels`).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SessionScopedModelWire {
    /// Serialized `Model` in the upstream shape.
    pub model: Value,
    /// Thinking level pinned by the scope, when present.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub thinking_level: Option<String>,
}

/// `session.update` event payload (Rust → host): the authoritative mirror
/// behind every synchronous session getter on the host.
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SessionStateWire {
    /// Session display name (`None` until set).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session_name: Option<String>,
    /// Current thinking level discriminant (`off` | `minimal` | …).
    pub thinking_level: String,
    /// Active tool names.
    pub active_tools: Vec<String>,
    /// All registered tools.
    pub all_tools: Vec<SessionToolWire>,
    /// Extension/prompt/skill slash-command catalog.
    pub commands: Vec<SessionCommandInfoWire>,
    /// Serialized active `Model` (upstream shape), if any.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<Value>,
    /// Models scoped to this session (`--models` / `enabledModels`).
    pub scoped_models: Vec<SessionScopedModelWire>,
    /// Whether the session has no active agent run.
    pub is_idle: bool,
    /// Whether steering/follow-up messages are queued.
    pub has_pending_messages: bool,
    /// Serialized `ContextUsage`, when computable.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub context_usage: Option<Value>,
    /// Current effective system prompt.
    pub system_prompt: String,
}

/// `session.command` event payload (host → Rust): one fire-and-forget
/// extension session action. Field names mirror the reference handler
/// signatures (`types.ts` `ExtensionActions` / `ExtensionContextActions`).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "action", rename_all = "camelCase")]
pub enum SessionCommand {
    /// `pi.sendMessage(message, options)`.
    SendMessage {
        /// `Pick<CustomMessage, "customType" | "content" | "display" | "details">`.
        message: Value,
        /// `{ triggerTurn?, deliverAs? }`.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        options: Option<Value>,
    },
    /// `pi.sendUserMessage(content, options)`.
    SendUserMessage {
        /// String or `(TextContent | ImageContent)[]`.
        content: Value,
        /// `{ deliverAs? }`.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        options: Option<Value>,
    },
    /// `pi.appendEntry(customType, data)`.
    AppendEntry {
        /// Custom entry type discriminant.
        #[serde(rename = "customType")]
        custom_type: String,
        /// Arbitrary payload.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        data: Option<Value>,
    },
    /// `pi.setSessionName(name)`.
    SetSessionName {
        /// New display name.
        name: String,
    },
    /// `pi.setLabel(entryId, label)`.
    SetLabel {
        /// Target session entry.
        #[serde(rename = "entryId")]
        entry_id: String,
        /// New label (`None` clears).
        #[serde(default, skip_serializing_if = "Option::is_none")]
        label: Option<String>,
    },
    /// `pi.setActiveTools(toolNames)`.
    SetActiveTools {
        /// Requested active tool names.
        #[serde(rename = "toolNames")]
        tool_names: Vec<String>,
    },
    /// `pi.refreshTools()`.
    RefreshTools,
    /// `pi.setThinkingLevel(level)`.
    SetThinkingLevel {
        /// Requested level discriminant.
        level: String,
    },
    /// `ctx.abort()`.
    Abort,
    /// `ctx.shutdown()`.
    Shutdown,
}

/// `session.command` event payload with optional pending-replacement scope.
///
/// Flattening preserves the original command object when no token is present;
/// candidate-session commands add only `replacementToken`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SessionCommandEnvelope {
    /// Token identifying the pending replacement session, when scoped.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub replacement_token: Option<String>,
    /// Existing flat `action`-tagged session command.
    #[serde(flatten)]
    pub command: SessionCommand,
}

/// Open method string: correlated `ctx.compact` request (host → Rust). The
/// response carries the serialized `CompactionResult`; failures arrive as a
/// protocol error frame (upstream delivers them to `onError`).
pub const SESSION_COMPACT_METHOD: &str = "session.compact";

/// `session.compact` request payload (host → Rust, correlated).
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SessionCompactRequest {
    /// Optional custom compaction instructions.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub custom_instructions: Option<String>,
}

/// `session.compact` response payload (Rust → host).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SessionCompactResponse {
    /// Serialized `CompactionResult` (upstream shape).
    pub result: Value,
}

/// `session.setModel` request payload (host → Rust, correlated).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SessionSetModelRequest {
    /// Serialized `Model` the extension asked to switch to.
    pub model: Value,
}

/// `session.setModel` response payload (Rust → host).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SessionSetModelResponse {
    /// Upstream `SetModelHandler` result: false when auth/persist failed.
    pub success: bool,
}

/// Open method string: correlated `ctx.newSession` request (host → Rust).
pub const SESSION_NEW_SESSION_METHOD: &str = "session.newSession";

/// Open method string: correlated `ctx.fork` request (host → Rust).
pub const SESSION_FORK_METHOD: &str = "session.fork";

/// Open method string: correlated `ctx.navigateTree` request (host → Rust).
pub const SESSION_NAVIGATE_TREE_METHOD: &str = "session.navigateTree";

/// Open method string: correlated `ctx.switchSession` request (host → Rust).
pub const SESSION_SWITCH_SESSION_METHOD: &str = "session.switchSession";

/// Open method string: correlated `ctx.reload` request (host → Rust).
pub const SESSION_RELOAD_METHOD: &str = "session.reload";

/// Open method string: correlated setup-entry snapshot request (host → Rust).
pub const SESSION_SETUP_ENTRIES_METHOD: &str = "session.setupEntries";

/// Open method string: host → Rust ready event after a replacement settles.
pub const SESSION_REPLACEMENT_READY_METHOD: &str = "session.replacementReady";

/// Open method string: host → Rust abort event for an abandoned replacement.
pub const SESSION_REPLACEMENT_ABORT_METHOD: &str = "session.replacementAbort";

/// Fork cut position relative to the target entry.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum SessionForkPosition {
    /// Cut before the entry (default when omitted downstream).
    Before,
    /// Cut at the entry.
    At,
}

/// `session.newSession` request payload (host → Rust, correlated).
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SessionNewSessionRequest {
    /// Optional parent session id / path for the new session.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parent_session: Option<String>,
}

/// `session.fork` request payload (host → Rust, correlated).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SessionForkRequest {
    /// Entry id to fork from.
    pub entry_id: String,
    /// Cut position; omitted maps to [`SessionForkPosition::Before`] downstream.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub position: Option<SessionForkPosition>,
}

/// `session.navigateTree` request payload (host → Rust, correlated).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SessionNavigateTreeRequest {
    /// Target entry / branch id.
    pub target_id: String,
    /// Whether to summarize the branch being left.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub summarize: Option<bool>,
    /// Optional custom summary instructions.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub custom_instructions: Option<String>,
    /// Whether custom instructions replace (vs append) the default ones.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub replace_instructions: Option<bool>,
    /// Optional label applied during navigation.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub label: Option<String>,
}

/// `session.switchSession` request payload (host → Rust, correlated).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SessionSwitchSessionRequest {
    /// Filesystem path of the session to switch to.
    pub session_path: String,
}

/// `session.reload` request payload (host → Rust, correlated).
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SessionReloadRequest {}

/// `session.newSession` response payload (Rust → host).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SessionNewSessionResponse {
    /// True when a before-hook cancelled the replacement.
    pub cancelled: bool,
    /// Ready-gate token; omitted when cancelled or ungated.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub replacement_token: Option<String>,
}

/// `session.fork` response payload (Rust → host).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SessionForkResponse {
    /// True when a before-hook cancelled the replacement.
    pub cancelled: bool,
    /// Editor selection captured at fork time.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub selected_text: Option<String>,
    /// Ready-gate token; omitted when cancelled or ungated.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub replacement_token: Option<String>,
}

/// `session.switchSession` response payload (Rust → host).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SessionSwitchSessionResponse {
    /// True when a before-hook cancelled the replacement.
    pub cancelled: bool,
    /// Ready-gate token; omitted when cancelled or ungated.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub replacement_token: Option<String>,
}

/// `session.navigateTree` response payload (Rust → host).
///
/// `summary_entry` is an opaque JSON value at the pi-ext boundary so this
/// crate does not depend on `pi`'s `SessionEntry`; the bridge arm serializes it.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SessionNavigateTreeResponse {
    /// True when a before-hook cancelled the navigation.
    pub cancelled: bool,
    /// Editor text restored after navigation.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub editor_text: Option<String>,
    /// True when summarization was aborted.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub aborted: Option<bool>,
    /// Opaque serialized branch-summary / session entry.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub summary_entry: Option<Value>,
}

/// `session.reload` response payload (Rust → host).
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SessionReloadResponse {
    /// Ready-gate token when the reload is pending host-side readiness.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub replacement_token: Option<String>,
}

/// Correlated setup-entry snapshot request (host → Rust).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SessionSetupEntriesRequest {
    /// Token returned by the initiating pending session replacement.
    pub replacement_token: String,
}

/// Correlated setup-entry snapshot response (Rust → host).
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SessionSetupEntriesResponse {
    /// Serialized `SessionEntry` values, opaque at the `pi-ext` boundary.
    pub entries: Vec<Value>,
}

/// `session.replacementReady` event payload (host → Rust).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SessionReplacementReadyEvent {
    /// Token previously returned on a replacement response.
    pub token: String,
}

/// `session.replacementAbort` event payload (host → Rust).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SessionReplacementAbortEvent {
    /// Token for the pending replacement that must be aborted.
    pub token: String,
}

/// `ui.control` event payload (host → Rust): one fire-and-forget
/// `ExtensionUIContext` data-surface control.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "control", rename_all = "camelCase")]
pub enum UiControl {
    /// `ui.setStatus(key, text)`; `None` clears the keyed entry.
    SetStatus {
        /// Status entry key.
        key: String,
        /// Status text (`None` clears).
        #[serde(default, skip_serializing_if = "Option::is_none")]
        text: Option<String>,
    },
    /// `ui.setWorkingMessage(message)`; `None` restores the default.
    SetWorkingMessage {
        /// Override for the streaming loader text.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        message: Option<String>,
    },
    /// `ui.setWorkingVisible(visible)`.
    SetWorkingVisible {
        /// Whether the working indicator is shown while streaming.
        visible: bool,
    },
    /// `ui.setWorkingIndicator(options)`; `None` restores the default.
    SetWorkingIndicator {
        /// `WorkingIndicatorOptions` (frames/intervalMs), upstream shape.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        options: Option<Value>,
    },
    /// `ui.setHiddenThinkingLabel(label)`; `None` restores the default.
    SetHiddenThinkingLabel {
        /// Label shown instead of hidden thinking blocks.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        label: Option<String>,
    },
    /// `ui.setTitle(title)`; `None` clears.
    SetTitle {
        /// Terminal title text.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        title: Option<String>,
    },
    /// `ui.pasteToEditor(text)`: insert at the cursor.
    PasteToEditor {
        /// Text to insert.
        text: String,
    },
    /// `ui.setEditorText(text)`: replace the editor content.
    SetEditorText {
        /// Replacement text.
        text: String,
    },
    /// `ui.setToolsExpanded(expanded)`.
    SetToolsExpanded {
        /// Whether tool blocks render expanded.
        expanded: bool,
    },
}

/// `ui.state` event payload (Rust → host): mirrored UI state behind the
/// synchronous `getEditorText` / `getToolsExpanded` getters. Pushed at UI
/// sync points (control application, submit, tool-expansion toggle), not per
/// keystroke.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct UiStateWire {
    /// Current editor content as of the last sync point.
    pub editor_text: String,
    /// Whether tool blocks render expanded.
    pub tools_expanded: bool,
}

/// Encode a frame to a single UTF-8 JSON line including the trailing `\n`.
///
/// # Errors
///
/// Returns [`ProtocolError::InvalidFrame`] when validation fails, or
/// [`ProtocolError::InvalidJson`] / [`ProtocolError::FrameTooLarge`] when the
/// encoded line is invalid or too large.
pub fn encode_frame(frame: &Frame) -> Result<Vec<u8>> {
    frame.validate(false)?;
    let mut bytes = serde_json::to_vec(frame)
        .map_err(|e| ProtocolError::InvalidJson(format!("encode frame: {e}")))?;
    if bytes.len() > MAX_FRAME_BYTES {
        return Err(ProtocolError::FrameTooLarge);
    }
    bytes.push(b'\n');
    Ok(bytes)
}

/// Encode a frame as a UTF-8 string including the trailing newline.
///
/// # Errors
///
/// Same as [`encode_frame`].
pub fn encode_frame_string(frame: &Frame) -> Result<String> {
    let bytes = encode_frame(frame)?;
    String::from_utf8(bytes).map_err(|e| ProtocolError::InvalidUtf8(e.to_string()))
}

/// Decode one complete JSON line (no trailing newline required) into a frame.
///
/// # Errors
///
/// Returns UTF-8, JSON, malformation, size, or validation errors.
pub fn decode_frame_line(line: &[u8]) -> Result<Frame> {
    if line.len() > MAX_FRAME_BYTES {
        return Err(ProtocolError::FrameTooLarge);
    }
    let text = str::from_utf8(line).map_err(|e| ProtocolError::InvalidUtf8(e.to_string()))?;
    decode_frame_str(text)
}

/// Decode one complete JSON line string into a frame.
///
/// # Errors
///
/// Returns JSON, malformation, size, or validation errors.
pub fn decode_frame_str(line: &str) -> Result<Frame> {
    let trimmed = line.trim_end_matches('\r');
    if trimmed.is_empty() {
        return Err(ProtocolError::MalformedFrame("empty line".to_owned()));
    }
    if trimmed.len() > MAX_FRAME_BYTES {
        return Err(ProtocolError::FrameTooLarge);
    }
    let frame: Frame =
        serde_json::from_str(trimmed).map_err(|e| ProtocolError::InvalidJson(e.to_string()))?;
    frame.validate(false)?;
    Ok(frame)
}

/// Decode and require an allowlisted method.
///
/// # Errors
///
/// Propagates [`decode_frame_str`] errors and [`ProtocolError::UnknownMethod`].
pub fn decode_frame_str_strict(line: &str) -> Result<Frame> {
    let frame = decode_frame_str(line)?;
    frame.validate(true)?;
    Ok(frame)
}

/// Incremental JSONL frame decoder with a hard size bound.
///
/// Accepts partial chunks, multiple frames per push, LF or CRLF separators,
/// and rejects oversize lines **before** the internal buffer can grow past
/// [`MAX_FRAME_BYTES`] + 1 pending newline scan window.
#[derive(Debug, Default)]
pub struct FrameDecoder {
    buf: Vec<u8>,
    max_frame_bytes: usize,
}

impl FrameDecoder {
    /// Create a decoder with the protocol default size limit.
    #[must_use]
    pub fn new() -> Self {
        Self {
            buf: Vec::new(),
            max_frame_bytes: MAX_FRAME_BYTES,
        }
    }

    /// Create a decoder with a custom max frame size (tests).
    #[must_use]
    pub fn with_max_frame_bytes(max_frame_bytes: usize) -> Self {
        Self {
            buf: Vec::new(),
            max_frame_bytes,
        }
    }

    /// Bytes currently buffered (incomplete line).
    #[must_use]
    pub fn buffered_len(&self) -> usize {
        self.buf.len()
    }

    /// Push bytes and return every complete frame decoded from this chunk.
    ///
    /// # Errors
    ///
    /// Returns the first size / UTF-8 / JSON / validation error encountered.
    /// On error, the decoder may drop the offending line and keep subsequent
    /// buffered data only when the error was per-line; oversize clears the
    /// current line buffer.
    pub fn push(&mut self, chunk: &[u8]) -> Result<Vec<Frame>> {
        let mut out = Vec::new();
        let mut offset = 0usize;
        while offset < chunk.len() {
            // Find next newline in chunk without copying the whole remainder.
            if let Some(rel) = chunk[offset..].iter().position(|&b| b == b'\n') {
                let line_end_in_chunk = offset + rel;
                let pending = self.buf.len() + (line_end_in_chunk - offset);
                if pending > self.max_frame_bytes {
                    self.buf.clear();
                    return Err(ProtocolError::FrameTooLarge);
                }
                self.buf
                    .extend_from_slice(&chunk[offset..line_end_in_chunk]);
                // Strip one trailing CR for CRLF.
                if self.buf.last() == Some(&b'\r') {
                    self.buf.pop();
                }
                let line = std::mem::take(&mut self.buf);
                out.push(decode_frame_line(&line)?);
                offset = line_end_in_chunk + 1;
            } else {
                let pending = self.buf.len() + (chunk.len() - offset);
                if pending > self.max_frame_bytes {
                    self.buf.clear();
                    return Err(ProtocolError::FrameTooLarge);
                }
                // Grow only up to the remaining allowed bytes.
                self.buf.extend_from_slice(&chunk[offset..]);
                break;
            }
        }
        Ok(out)
    }

    /// Finish the stream: error if a partial line remains; `Ok(None)` if empty.
    ///
    /// # Errors
    ///
    /// Returns [`ProtocolError::Truncated`] when buffered bytes remain, or
    /// decode errors if a final line without newline should be accepted — this
    /// API requires newline-terminated frames, so remainder is truncated.
    pub fn finish(&mut self) -> Result<Option<Frame>> {
        if self.buf.is_empty() {
            return Ok(None);
        }
        // Final non-empty buffer without newline is a truncated frame.
        let leftover = std::mem::take(&mut self.buf);
        if leftover.iter().all(u8::is_ascii_whitespace) {
            return Ok(None);
        }
        Err(ProtocolError::Truncated)
    }

    /// Finish accepting a final line without a trailing newline (EOF flush).
    ///
    /// # Errors
    ///
    /// Returns decode errors for the final line, or [`ProtocolError::FrameTooLarge`].
    pub fn finish_with_final_line(&mut self) -> Result<Option<Frame>> {
        if self.buf.is_empty() {
            return Ok(None);
        }
        if self.buf.len() > self.max_frame_bytes {
            self.buf.clear();
            return Err(ProtocolError::FrameTooLarge);
        }
        if self.buf.last() == Some(&b'\r') {
            self.buf.pop();
        }
        let line = std::mem::take(&mut self.buf);
        if line.is_empty() {
            return Ok(None);
        }
        Ok(Some(decode_frame_line(&line)?))
    }

    /// Reset buffered state.
    pub fn reset(&mut self) {
        self.buf.clear();
    }
}

/// Serialize a typed payload into a JSON object value.
///
/// # Errors
///
/// Returns [`ProtocolError::InvalidJson`] on serialization failure.
pub fn to_payload<T: Serialize>(value: &T) -> Result<Value> {
    serde_json::to_value(value).map_err(|e| ProtocolError::InvalidJson(e.to_string()))
}

/// Deserialize a typed payload from a frame payload value.
///
/// # Errors
///
/// Returns [`ProtocolError::InvalidJson`] on deserialization failure.
pub fn from_payload<T: for<'de> Deserialize<'de>>(payload: &Value) -> Result<T> {
    serde_json::from_value(payload.clone()).map_err(|e| ProtocolError::InvalidJson(e.to_string()))
}

/// Empty object payload helper.
#[must_use]
pub fn empty_object() -> Value {
    Value::Object(Map::new())
}

#[cfg(test)]
mod tests {
    use super::*;

    const FIXTURES: &str =
        include_str!("../../../packages/pi-tui-protocol/tests/fixtures/frames.jsonl");

    type TestResult = std::result::Result<(), Box<dyn std::error::Error>>;

    fn sample_hello_req() -> Result<Frame> {
        Ok(Frame::request(
            1,
            Method::Hello,
            to_payload(&Hello::local())?,
        ))
    }

    #[test]
    fn versions_are_stable() {
        assert_eq!(PROTOCOL_VERSION, 1);
        assert_eq!(COMPATIBILITY_VERSION, "0.80.10");
        assert_eq!(MAX_FRAME_BYTES, 8 * 1024 * 1024);
    }

    #[test]
    fn method_allowlist_roundtrip() {
        for method in Method::ALL {
            assert_eq!(Method::parse(method.as_str()), Some(*method));
        }
        assert!(Method::parse("notAMethod").is_none());
    }

    #[test]
    fn frame_id_rules() -> TestResult {
        let mut frame = sample_hello_req()?;
        frame.id = 0;
        assert!(matches!(
            frame.validate(false),
            Err(ProtocolError::InvalidFrame(_))
        ));
        Frame::event(0, Method::Notify, empty_object()).validate(false)?;
        Ok(())
    }

    #[test]
    fn hello_version_gate() -> TestResult {
        Hello::local().validate_remote()?;
        let bad = Hello {
            protocol_version: 99,
            compatibility_version: COMPATIBILITY_VERSION.to_owned(),
        };
        assert!(matches!(
            bad.validate_remote(),
            Err(ProtocolError::VersionMismatch {
                remote: 99,
                local: 1
            })
        ));
        let bad_compat = Hello {
            protocol_version: 1,
            compatibility_version: "0.0.0".to_owned(),
        };
        assert!(matches!(
            bad_compat.validate_remote(),
            Err(ProtocolError::CompatibilityMismatch { .. })
        ));
        Ok(())
    }

    #[test]
    fn encode_decode_roundtrip_typed() -> TestResult {
        let hello = sample_hello_req()?;
        let line = encode_frame_string(&hello)?;
        assert!(line.ends_with('\n'));
        let decoded = decode_frame_str(line.trim_end())?;
        assert_eq!(decoded, hello);
        assert_eq!(from_payload::<Hello>(&decoded.payload)?, Hello::local());

        let ack = Frame::response(1, Method::Hello, to_payload(&HelloAck::local())?);
        let ack_line = encode_frame_string(&ack)?;
        let decoded_ack = decode_frame_str(ack_line.trim_end())?;
        assert_eq!(
            from_payload::<HelloAck>(&decoded_ack.payload)?,
            HelloAck::local()
        );
        Ok(())
    }

    fn sample_slot() -> UiSlot {
        UiSlot {
            key: "widget.demo".to_owned(),
            generation: 3,
            placement: SlotPlacement::AboveEditor,
            height: 2,
            runs: vec![
                vec![StyledRun {
                    text: "hi".to_owned(),
                    style: Style {
                        bold: Some(true),
                        fg: Some(WireColor::Named {
                            name: NamedColor::Green,
                        }),
                        ..Style::default()
                    },
                }],
                vec![StyledRun {
                    text: "link".to_owned(),
                    style: Style {
                        underline: Some(true),
                        link: Some(Hyperlink {
                            id: Some("a".to_owned()),
                            uri: "https://example.com".to_owned(),
                        }),
                        fg: Some(WireColor::Rgb { r: 1, g: 2, b: 3 }),
                        ..Style::default()
                    },
                }],
            ],
            focusable: true,
            cursor: Some(SlotCursor { col: 1, row: 0 }),
            overlay_options: Some(OverlaySpec {
                width: Some(SizeValue::Percent(50)),
                anchor: Some(OverlayAnchor::TopCenter),
                margin: Some(OverlayMarginWire::Uniform(2)),
                non_capturing: true,
                ..OverlaySpec::default()
            }),
        }
    }

    #[test]
    fn ui_slot_and_style_roundtrip() -> TestResult {
        let slot = sample_slot();
        let frame = Frame::event(0, Method::UiSlot, to_payload(&slot)?);
        let line = encode_frame_string(&frame)?;
        let decoded = decode_frame_str(line.trim_end())?;
        let back: UiSlot = from_payload(&decoded.payload)?;
        assert_eq!(back, slot);
        back.validate()?;
        Ok(())
    }

    #[test]
    fn overlay_margin_accepts_uniform_and_sides() -> TestResult {
        let uniform: OverlaySpec = serde_json::from_value(serde_json::json!({"margin": 3}))?;
        assert_eq!(uniform.margin, Some(OverlayMarginWire::Uniform(3)));

        let sides: OverlaySpec = serde_json::from_value(serde_json::json!({
            "margin": {"top": 1, "right": 2, "bottom": 3, "left": 4}
        }))?;
        assert_eq!(
            sides.margin,
            Some(OverlayMarginWire::Sides(OverlayMargin {
                top: 1,
                right: 2,
                bottom: 3,
                left: 4,
            }))
        );
        Ok(())
    }

    #[test]
    fn direct_overlay_margin_deserializes_scalar_as_uniform() -> TestResult {
        let m: OverlayMargin = serde_json::from_str("4")?;
        assert_eq!(m, OverlayMargin::uniform(4));
        let m2: OverlayMargin = serde_json::from_value(serde_json::Value::Number(7.into()))?;
        assert_eq!(m2, OverlayMargin::uniform(7));
        Ok(())
    }

    #[test]
    fn direct_overlay_margin_deserializes_partial_object_defaults_zero() -> TestResult {
        let m: OverlayMargin = serde_json::from_str(r#"{"top":1}"#)?;
        assert_eq!(
            m,
            OverlayMargin {
                top: 1,
                right: 0,
                bottom: 0,
                left: 0
            }
        );
        Ok(())
    }

    #[test]
    fn direct_overlay_margin_serializes_as_normalized_object() -> TestResult {
        let m = OverlayMargin::uniform(4);
        let out = serde_json::to_value(m)?;
        assert!(out.is_object());
        assert_eq!(out["top"], 4);
        assert_eq!(out["right"], 4);
        assert_eq!(out["bottom"], 4);
        assert_eq!(out["left"], 4);
        // A scalar input round-trips through the normalized object form.
        let back: OverlayMargin = serde_json::from_value(out)?;
        assert_eq!(back, OverlayMargin::uniform(4));
        Ok(())
    }

    #[test]
    fn direct_overlay_margin_rejects_negative_scalar() {
        assert!(serde_json::from_str::<OverlayMargin>("-1").is_err());
    }

    #[test]
    fn overlay_spec_keeps_portable_scalar_form_via_wire() -> TestResult {
        // OverlaySpec must retain OverlayMarginWire so the scalar round-trip
        // stays part of the shared Rust/TypeScript fixture contract.
        let spec: OverlaySpec = serde_json::from_value(serde_json::json!({"margin": 5}))?;
        assert_eq!(spec.margin, Some(OverlayMarginWire::Uniform(5)));
        let encoded = serde_json::to_value(&spec)?;
        assert_eq!(encoded["margin"], 5);
        Ok(())
    }

    #[test]
    fn ui_slot_rejects_forbidden_and_oversized_links() {
        for link in [
            serde_json::json!({"uri": "javascript:alert(1)"}),
            serde_json::json!({"uri": "file:///tmp/x"}),
            serde_json::json!({"uri": format!("https://example.com/{}", "x".repeat(2048))}),
            serde_json::json!({"id": "x".repeat(129), "uri": "https://example.com"}),
        ] {
            let frame = Frame::event(
                0,
                Method::UiSlot,
                serde_json::json!({
                    "key": "bad",
                    "generation": 1,
                    "placement": "aboveEditor",
                    "height": 1,
                    "runs": [[{"text": "bad", "style": {"link": link}}]]
                }),
            );
            assert!(matches!(
                frame.validate(false),
                Err(ProtocolError::InvalidFrame(_))
            ));
        }
    }

    #[test]
    fn dialog_payloads_roundtrip() -> TestResult {
        let select = SelectRequest {
            title: "Pick".to_owned(),
            options: vec!["a".to_owned(), "b".to_owned()],
            options_meta: DialogOptions {
                timeout_ms: Some(1000),
            },
        };
        let frame = Frame::request(7, Method::Select, to_payload(&select)?);
        let line = encode_frame_string(&frame)?;
        let decoded = decode_frame_str(line.trim_end())?;
        assert_eq!(from_payload::<SelectRequest>(&decoded.payload)?, select);

        let confirm = ConfirmResponse { confirmed: true };
        let frame = Frame::response(7, Method::Confirm, to_payload(&confirm)?);
        let line = encode_frame_string(&frame)?;
        let decoded = decode_frame_str(line.trim_end())?;
        assert!(from_payload::<ConfirmResponse>(&decoded.payload)?.confirmed);
        Ok(())
    }

    #[test]
    fn ui_event_wire_variants() -> TestResult {
        let events = [
            UiEventWire::Key {
                code: "enter".to_owned(),
                modifiers: KeyModifiersWire {
                    ctrl: Some(true),
                    ..KeyModifiersWire::default()
                },
                kind: KeyEventKindWire::Press,
            },
            UiEventWire::Paste {
                text: "a\nb".to_owned(),
            },
            UiEventWire::FocusGained,
            UiEventWire::FocusLost,
            UiEventWire::Resize {
                width: 80,
                height: 24,
            },
        ];
        for event in events {
            let frame = Frame::request(2, Method::UiEvent, to_payload(&event)?);
            let line = encode_frame_string(&frame)?;
            let decoded = decode_frame_str(line.trim_end())?;
            assert_eq!(from_payload::<UiEventWire>(&decoded.payload)?, event);
        }
        Ok(())
    }

    #[test]
    fn decoder_fragmentation_and_multiple() -> TestResult {
        let first = sample_hello_req()?;
        let second = Frame::response(1, Method::Hello, to_payload(&HelloAck::local())?);
        let mut bytes = encode_frame(&first)?;
        bytes.extend(encode_frame(&second)?);
        let mut decoder = FrameDecoder::new();
        let mut got = Vec::new();
        for byte in bytes {
            got.extend(decoder.push(&[byte])?);
        }
        assert!(decoder.finish()?.is_none());
        assert_eq!(got, vec![first, second]);
        Ok(())
    }

    #[test]
    fn decoder_crlf() -> TestResult {
        let frame = sample_hello_req()?;
        let mut line = serde_json::to_vec(&frame)?;
        line.extend_from_slice(b"\r\n");
        let mut decoder = FrameDecoder::new();
        let got = decoder.push(&line)?;
        assert_eq!(got.first(), Some(&frame));
        assert_eq!(got.len(), 1);
        Ok(())
    }

    #[test]
    fn decoder_final_line_without_newline() -> TestResult {
        let frame = sample_hello_req()?;
        let line = serde_json::to_vec(&frame)?;
        let mut decoder = FrameDecoder::new();
        assert!(decoder.push(&line)?.is_empty());
        assert_eq!(decoder.finish_with_final_line()?, Some(frame));

        let mut strict = FrameDecoder::new();
        assert!(strict.push(&line)?.is_empty());
        assert!(matches!(strict.finish(), Err(ProtocolError::Truncated)));
        Ok(())
    }

    #[test]
    fn decoder_invalid_utf8_and_json() {
        let mut decoder = FrameDecoder::new();
        assert!(matches!(
            decoder.push(b"\xff\n"),
            Err(ProtocolError::InvalidUtf8(_))
        ));
        let mut decoder = FrameDecoder::new();
        assert!(matches!(
            decoder.push(b"{not-json}\n"),
            Err(ProtocolError::InvalidJson(_))
        ));
    }

    #[test]
    fn decoder_oversized_before_growth() -> TestResult {
        let limit = 64;
        let mut decoder = FrameDecoder::with_max_frame_bytes(limit);
        assert_eq!(
            decoder.push(&vec![b'a'; limit + 1]),
            Err(ProtocolError::FrameTooLarge)
        );
        assert_eq!(decoder.buffered_len(), 0);

        let mut decoder = FrameDecoder::with_max_frame_bytes(limit);
        assert!(decoder.push(&vec![b'b'; limit / 2])?.is_empty());
        assert_eq!(
            decoder.push(&vec![b'c'; limit]),
            Err(ProtocolError::FrameTooLarge)
        );
        Ok(())
    }

    #[test]
    fn strict_unknown_method() -> TestResult {
        let frame = Frame {
            id: 1,
            kind: FrameKind::Req,
            method: "notAllowlisted".to_owned(),
            payload: empty_object(),
        };
        let line = encode_frame_string(&frame)?;
        assert!(decode_frame_str(line.trim_end()).is_ok());
        assert!(matches!(
            decode_frame_str_strict(line.trim_end()),
            Err(ProtocolError::UnknownMethod(_))
        ));
        Ok(())
    }

    #[test]
    fn error_payload_shape() -> TestResult {
        let error = ErrorPayload {
            code: "extension_error".to_owned(),
            message: "boom".to_owned(),
            retryable: false,
            data: Some(serde_json::json!({"path": "x.ts"})),
        };
        let frame = Frame::error_frame(9, Method::ExtensionError, &error)?;
        let line = encode_frame_string(&frame)?;
        let decoded = decode_frame_str(line.trim_end())?;
        assert_eq!(from_payload::<ErrorPayload>(&decoded.payload)?, error);
        Ok(())
    }

    #[test]
    fn shared_fixtures_field_and_discriminant_parity() -> TestResult {
        let mut count = 0usize;
        for line in FIXTURES.lines() {
            if line.trim().is_empty() || line.trim_start().starts_with('#') {
                continue;
            }
            let frame = decode_frame_str(line)?;
            let encoded = encode_frame_string(&frame)?;
            let again = decode_frame_str(encoded.trim_end())?;
            assert_eq!(again, frame);
            if let Some(method) = frame.method_enum() {
                assert!(Method::ALL.contains(&method));
            }
            count += 1;
        }
        assert!(count >= 8);
        Ok(())
    }

    #[test]
    fn eight_mib_limit_constant_and_encode_guard() {
        let frame = Frame {
            id: 1,
            kind: FrameKind::Req,
            method: Method::Notify.as_str().to_owned(),
            payload: serde_json::json!({"blob": "x".repeat(MAX_FRAME_BYTES)}),
        };
        assert_eq!(encode_frame(&frame), Err(ProtocolError::FrameTooLarge));
    }

    #[test]
    fn hyperlink_validation() -> TestResult {
        Hyperlink {
            id: None,
            uri: "https://ok".to_owned(),
        }
        .validate()?;
        assert!(
            Hyperlink {
                id: None,
                uri: "javascript:alert(1)".to_owned(),
            }
            .validate()
            .is_err()
        );
        assert!(
            Hyperlink {
                id: Some("a".repeat(Hyperlink::MAX_ID_BYTES + 1)),
                uri: "https://ok".to_owned(),
            }
            .validate()
            .is_err()
        );
        Ok(())
    }

    #[test]
    fn extension_control_payloads_roundtrip() -> TestResult {
        let flags = FlagsSetRequest {
            values: BTreeMap::from([
                ("plan".to_owned(), FlagValueWire::Boolean(true)),
                (
                    "profile".to_owned(),
                    FlagValueWire::String("fast".to_owned()),
                ),
            ]),
        };
        let _payload = to_payload(&flags)?;
        let shortcut = ShortcutExecuteRequest {
            key: "ctrl+alt+p".to_owned(),
        };
        let payload = to_payload(&shortcut)?;
        assert_eq!(from_payload::<ShortcutExecuteRequest>(&payload)?, shortcut);

        let ui = UiEventRequest {
            key: "overlay.1".to_owned(),
            generation: 2,
            event: UiEventWire::Paste {
                text: "hello".to_owned(),
            },
            data: Some("hello".to_owned()),
        };
        let frame = Frame::request(9, Method::UiEvent, to_payload(&ui)?);
        let decoded = decode_frame_str(encode_frame_string(&frame)?.trim_end())?;
        assert_eq!(from_payload::<UiEventRequest>(&decoded.payload)?, ui);
        Ok(())
    }
    #[test]
    fn theme_wire_payloads_roundtrip_with_json_color_vocabulary() -> TestResult {
        let mut fg = BTreeMap::new();
        fg.insert(
            "text".to_owned(),
            ThemeColorValue::Text("#ededed".to_owned()),
        );
        fg.insert("accent".to_owned(), ThemeColorValue::Index(39));
        fg.insert("muted".to_owned(), ThemeColorValue::Text(String::new()));
        let mut bg = BTreeMap::new();
        bg.insert(
            "selectedBg".to_owned(),
            ThemeColorValue::Text("#0a0a0a".to_owned()),
        );
        let update = ThemeUpdate {
            theme: ThemeWire {
                name: Some("dark".to_owned()),
                source_path: Some("/pkg/theme/dark.json".to_owned()),
                color_mode: "truecolor".to_owned(),
                fg,
                bg,
            },
            terminal_theme: "light".to_owned(),
            theme_mode: "auto".to_owned(),
            theme_generation: 42,
            themes: vec![ThemeCatalogEntry {
                name: "mytheme".to_owned(),
                path: Some("/agent/themes/my-file.json".to_owned()),
                file_stem: Some("my-file".to_owned()),
                theme: ThemeWire {
                    name: Some("mytheme".to_owned()),
                    source_path: None,
                    color_mode: "256color".to_owned(),
                    fg: BTreeMap::new(),
                    bg: BTreeMap::new(),
                },
            }],
        };
        let payload = to_payload(&update)?;
        // Slot values serialize as bare JSON scalars (upstream ColorValue).
        assert_eq!(payload["theme"]["fg"]["text"], "#ededed");
        assert_eq!(payload["theme"]["fg"]["accent"], 39);
        assert_eq!(payload["theme"]["fg"]["muted"], "");
        assert_eq!(payload["themeGeneration"], 42);
        assert_eq!(payload["themes"][0]["fileStem"], "my-file");
        assert_eq!(from_payload::<ThemeUpdate>(&payload)?, update);

        let set = ThemeSet {
            name: Some("light/dark".to_owned()),
            theme: None,
            persist: true,
        };
        let payload = to_payload(&set)?;
        assert_eq!(payload["name"], "light/dark");
        assert!(payload.get("theme").is_none());
        assert_eq!(from_payload::<ThemeSet>(&payload)?, set);
        Ok(())
    }
}

#[cfg(test)]
mod bridge_tests {
    use super::*;
    use crate::adapters::methods;
    use crate::server::RegistrySnapshot;

    type TestResult = std::result::Result<(), Box<dyn std::error::Error>>;

    const FIXTURES: &str =
        include_str!("../../../packages/pi-tui-protocol/tests/fixtures/frames.jsonl");

    /// Every open bridge method must be locked into the shared witness, and
    /// each witnessed payload must decode through its typed wire struct.
    #[allow(clippy::too_many_lines)]
    #[test]
    fn shared_fixtures_cover_bridge_methods_typed() -> TestResult {
        let mut seen: std::collections::HashSet<(String, FrameKind)> =
            std::collections::HashSet::new();
        for line in FIXTURES.lines() {
            if line.trim().is_empty() || line.trim_start().starts_with('#') {
                continue;
            }
            let frame = decode_frame_str(line)?;
            match frame.method.as_str() {
                THEME_UPDATE_METHOD => {
                    from_payload::<ThemeUpdate>(&frame.payload)?;
                }
                THEME_SET_METHOD => {
                    from_payload::<ThemeSet>(&frame.payload)?;
                }
                SESSION_UPDATE_METHOD => {
                    from_payload::<SessionStateWire>(&frame.payload)?;
                }
                SESSION_COMMAND_METHOD => {
                    from_payload::<SessionCommandEnvelope>(&frame.payload)?;
                }
                SESSION_SET_MODEL_METHOD
                | SESSION_COMPACT_METHOD
                | SESSION_NEW_SESSION_METHOD
                | SESSION_FORK_METHOD
                | SESSION_NAVIGATE_TREE_METHOD
                | SESSION_SWITCH_SESSION_METHOD
                | SESSION_RELOAD_METHOD
                | SESSION_SETUP_ENTRIES_METHOD
                    if frame.kind == FrameKind::Error =>
                {
                    from_payload::<ErrorPayload>(&frame.payload)?;
                }
                SESSION_SET_MODEL_METHOD if frame.kind == FrameKind::Req => {
                    from_payload::<SessionSetModelRequest>(&frame.payload)?;
                }
                SESSION_SET_MODEL_METHOD => {
                    from_payload::<SessionSetModelResponse>(&frame.payload)?;
                }
                SESSION_COMPACT_METHOD if frame.kind == FrameKind::Req => {
                    from_payload::<SessionCompactRequest>(&frame.payload)?;
                }
                SESSION_COMPACT_METHOD => {
                    from_payload::<SessionCompactResponse>(&frame.payload)?;
                }
                SESSION_NEW_SESSION_METHOD if frame.kind == FrameKind::Req => {
                    from_payload::<SessionNewSessionRequest>(&frame.payload)?;
                }
                SESSION_NEW_SESSION_METHOD if frame.kind == FrameKind::Res => {
                    from_payload::<SessionNewSessionResponse>(&frame.payload)?;
                }
                SESSION_FORK_METHOD if frame.kind == FrameKind::Req => {
                    from_payload::<SessionForkRequest>(&frame.payload)?;
                }
                SESSION_FORK_METHOD => {
                    from_payload::<SessionForkResponse>(&frame.payload)?;
                }
                SESSION_NAVIGATE_TREE_METHOD if frame.kind == FrameKind::Req => {
                    from_payload::<SessionNavigateTreeRequest>(&frame.payload)?;
                }
                SESSION_NAVIGATE_TREE_METHOD => {
                    from_payload::<SessionNavigateTreeResponse>(&frame.payload)?;
                }
                SESSION_SWITCH_SESSION_METHOD if frame.kind == FrameKind::Req => {
                    from_payload::<SessionSwitchSessionRequest>(&frame.payload)?;
                }
                SESSION_SWITCH_SESSION_METHOD => {
                    from_payload::<SessionSwitchSessionResponse>(&frame.payload)?;
                }
                SESSION_RELOAD_METHOD if frame.kind == FrameKind::Req => {
                    from_payload::<SessionReloadRequest>(&frame.payload)?;
                }
                SESSION_RELOAD_METHOD => {
                    from_payload::<SessionReloadResponse>(&frame.payload)?;
                }
                SESSION_SETUP_ENTRIES_METHOD if frame.kind == FrameKind::Req => {
                    from_payload::<SessionSetupEntriesRequest>(&frame.payload)?;
                }
                SESSION_SETUP_ENTRIES_METHOD => {
                    from_payload::<SessionSetupEntriesResponse>(&frame.payload)?;
                }
                SESSION_REPLACEMENT_READY_METHOD => {
                    from_payload::<SessionReplacementReadyEvent>(&frame.payload)?;
                }
                SESSION_REPLACEMENT_ABORT_METHOD => {
                    from_payload::<SessionReplacementAbortEvent>(&frame.payload)?;
                }
                UI_CONTROL_METHOD => {
                    from_payload::<UiControl>(&frame.payload)?;
                }
                UI_STATE_METHOD => {
                    from_payload::<UiStateWire>(&frame.payload)?;
                }
                PROVIDERS_UPDATE_METHOD => {
                    from_payload::<ProvidersUpdate>(&frame.payload)?;
                }
                _ => continue,
            }
            seen.insert((frame.method, frame.kind));
        }
        for (method, kind) in [
            (THEME_UPDATE_METHOD, FrameKind::Event),
            (THEME_SET_METHOD, FrameKind::Event),
            (SESSION_UPDATE_METHOD, FrameKind::Event),
            (SESSION_COMMAND_METHOD, FrameKind::Event),
            (SESSION_SET_MODEL_METHOD, FrameKind::Req),
            (SESSION_SET_MODEL_METHOD, FrameKind::Error),
            (SESSION_SET_MODEL_METHOD, FrameKind::Res),
            (SESSION_COMPACT_METHOD, FrameKind::Req),
            (SESSION_COMPACT_METHOD, FrameKind::Error),
            (SESSION_COMPACT_METHOD, FrameKind::Res),
            (SESSION_NEW_SESSION_METHOD, FrameKind::Req),
            (SESSION_NEW_SESSION_METHOD, FrameKind::Res),
            (SESSION_NEW_SESSION_METHOD, FrameKind::Error),
            (SESSION_FORK_METHOD, FrameKind::Req),
            (SESSION_FORK_METHOD, FrameKind::Error),
            (SESSION_FORK_METHOD, FrameKind::Res),
            (SESSION_NAVIGATE_TREE_METHOD, FrameKind::Req),
            (SESSION_NAVIGATE_TREE_METHOD, FrameKind::Error),
            (SESSION_NAVIGATE_TREE_METHOD, FrameKind::Res),
            (SESSION_SWITCH_SESSION_METHOD, FrameKind::Req),
            (SESSION_SWITCH_SESSION_METHOD, FrameKind::Error),
            (SESSION_SWITCH_SESSION_METHOD, FrameKind::Res),
            (SESSION_RELOAD_METHOD, FrameKind::Req),
            (SESSION_RELOAD_METHOD, FrameKind::Error),
            (SESSION_RELOAD_METHOD, FrameKind::Res),
            (SESSION_SETUP_ENTRIES_METHOD, FrameKind::Req),
            (SESSION_SETUP_ENTRIES_METHOD, FrameKind::Error),
            (SESSION_SETUP_ENTRIES_METHOD, FrameKind::Res),
            (PROVIDERS_UPDATE_METHOD, FrameKind::Event),
            (SESSION_REPLACEMENT_READY_METHOD, FrameKind::Event),
            (SESSION_REPLACEMENT_ABORT_METHOD, FrameKind::Event),
            (UI_STATE_METHOD, FrameKind::Event),
        ] {
            assert!(
                seen.contains(&(method.to_owned(), kind)),
                "fixture missing {method} {kind} frame"
            );
        }
        Ok(())
    }

    #[test]
    fn shared_fixtures_require_legacy_and_tagged_session_command_witnesses() -> TestResult {
        let mut legacy = std::collections::HashSet::<String>::new();
        let mut candidate = std::collections::HashSet::<String>::new();
        for line in FIXTURES.lines() {
            if line.trim().is_empty() || line.trim_start().starts_with('#') {
                continue;
            }
            let frame = decode_frame_str(line)?;
            if frame.method != SESSION_COMMAND_METHOD || frame.kind != FrameKind::Event {
                continue;
            }
            let envelope = from_payload::<SessionCommandEnvelope>(&frame.payload)?;
            let action = frame.payload["action"].as_str().ok_or_else(|| {
                "session.command fixture is missing a string `action` field".to_owned()
            })?;
            if envelope.replacement_token.is_some() {
                candidate.insert(action.to_owned());
            } else {
                legacy.insert(action.to_owned());
            }
        }

        let expected: std::collections::HashSet<String> =
            ["setSessionName", "sendMessage", "shutdown"]
                .iter()
                .map(|&s| s.to_owned())
                .collect();

        assert_eq!(
            legacy, expected,
            "legacy untagged session.command action set does not match fixture"
        );
        assert_eq!(
            candidate, expected,
            "candidate-tagged session.command action set does not match fixture"
        );
        Ok(())
    }

    #[test]
    fn session_command_wire_discriminants() -> TestResult {
        let cmd = SessionCommand::SetLabel {
            entry_id: "e1".to_owned(),
            label: None,
        };
        let payload = to_payload(&cmd)?;
        assert_eq!(payload["action"], "setLabel");
        assert_eq!(payload["entryId"], "e1");
        assert!(payload.get("label").is_none());
        assert_eq!(from_payload::<SessionCommand>(&payload)?, cmd);

        let cmd = SessionCommand::SetActiveTools {
            tool_names: vec!["read".to_owned()],
        };
        let payload = to_payload(&cmd)?;
        assert_eq!(payload["action"], "setActiveTools");
        assert_eq!(payload["toolNames"][0], "read");

        let request = SessionCompactRequest {
            custom_instructions: Some("keep decisions".to_owned()),
        };
        let payload = to_payload(&request)?;
        assert_eq!(payload["customInstructions"], "keep decisions");
        assert_eq!(from_payload::<SessionCompactRequest>(&payload)?, request);
        Ok(())
    }

    #[test]
    fn session_command_envelope_preserves_untagged_and_tagged_commands() -> TestResult {
        let ordinary_payload = serde_json::json!({
            "action": "setSessionName",
            "name": "Renamed"
        });
        let ordinary = from_payload::<SessionCommandEnvelope>(&ordinary_payload)?;
        assert_eq!(ordinary.replacement_token, None);
        assert_eq!(
            ordinary.command,
            SessionCommand::SetSessionName {
                name: "Renamed".to_owned(),
            }
        );
        assert_eq!(to_payload(&ordinary)?, ordinary_payload);

        let candidate_payload = serde_json::json!({
            "replacementToken": "tok-1",
            "action": "setSessionName",
            "name": "Candidate"
        });
        let candidate = from_payload::<SessionCommandEnvelope>(&candidate_payload)?;
        assert_eq!(candidate.replacement_token.as_deref(), Some("tok-1"));
        assert_eq!(to_payload(&candidate)?, candidate_payload);
        Ok(())
    }

    #[test]
    fn session_replacement_abort_has_canonical_shape() -> TestResult {
        let event = SessionReplacementAbortEvent {
            token: "tok-closed".to_owned(),
        };
        let payload = to_payload(&event)?;
        assert_eq!(payload, serde_json::json!({"token": "tok-closed"}));
        assert_eq!(
            from_payload::<SessionReplacementAbortEvent>(&payload)?,
            event
        );
        Ok(())
    }

    #[test]
    fn ui_control_wire_discriminants() -> TestResult {
        let control = UiControl::SetStatus {
            key: "lint".to_owned(),
            text: None,
        };
        let payload = to_payload(&control)?;
        assert_eq!(payload["control"], "setStatus");
        assert!(payload.get("text").is_none());
        assert_eq!(from_payload::<UiControl>(&payload)?, control);

        let control = UiControl::SetToolsExpanded { expanded: true };
        let payload = to_payload(&control)?;
        assert_eq!(payload["control"], "setToolsExpanded");
        assert_eq!(payload["expanded"], true);
        Ok(())
    }

    #[test]
    fn session_state_wire_roundtrip() -> TestResult {
        let state = SessionStateWire {
            session_name: Some("s".to_owned()),
            thinking_level: "high".to_owned(),
            active_tools: vec!["read".to_owned()],
            all_tools: vec![SessionToolWire {
                name: "read".to_owned(),
                description: "Read".to_owned(),
                parameters: serde_json::json!({"type": "object"}),
                source: Some("builtin".to_owned()),
            }],
            commands: vec![SessionCommandInfoWire {
                name: "review".to_owned(),
                description: None,
                source: "extension".to_owned(),
            }],
            model: None,
            scoped_models: vec![
                SessionScopedModelWire {
                    model: serde_json::json!({"id": "gpt-x", "provider": "openai"}),
                    thinking_level: Some("high".to_owned()),
                },
                SessionScopedModelWire {
                    model: serde_json::json!({"id": "haiku", "provider": "anthropic"}),
                    thinking_level: None,
                },
            ],
            is_idle: false,
            has_pending_messages: true,
            context_usage: None,
            system_prompt: "p".to_owned(),
        };
        let payload = to_payload(&state)?;
        assert_eq!(payload["thinkingLevel"], "high");
        assert_eq!(payload["hasPendingMessages"], true);
        assert_eq!(payload["scopedModels"][0]["thinkingLevel"], "high");
        assert!(payload["scopedModels"][1].get("thinkingLevel").is_none());
        assert!(payload.get("model").is_none());
        assert_eq!(from_payload::<SessionStateWire>(&payload)?, state);
        Ok(())
    }

    #[test]
    fn session_setup_entries_wire_roundtrip() -> TestResult {
        let request = SessionSetupEntriesRequest {
            replacement_token: "tok-1".to_owned(),
        };
        let payload = to_payload(&request)?;
        assert_eq!(payload["replacementToken"], "tok-1");
        assert_eq!(
            from_payload::<SessionSetupEntriesRequest>(&payload)?,
            request
        );

        let response = SessionSetupEntriesResponse {
            entries: vec![
                serde_json::json!({"type": "session_info", "id": "e1"}),
                serde_json::json!({"type": "custom", "id": "e2"}),
            ],
        };
        let payload = to_payload(&response)?;
        assert_eq!(payload["entries"].as_array().map(Vec::len), Some(2));
        assert_eq!(
            from_payload::<SessionSetupEntriesResponse>(&payload)?,
            response
        );
        Ok(())
    }

    #[test]
    fn session_replacement_wire_optional_fields() -> TestResult {
        let cancelled = SessionNewSessionResponse {
            cancelled: true,
            replacement_token: None,
        };
        let payload = to_payload(&cancelled)?;
        assert_eq!(payload["cancelled"], true);
        assert!(payload.get("replacementToken").is_none());
        assert_eq!(
            from_payload::<SessionNewSessionResponse>(&payload)?,
            cancelled
        );

        let full = SessionNavigateTreeResponse {
            cancelled: false,
            editor_text: Some("draft".to_owned()),
            aborted: Some(false),
            summary_entry: Some(serde_json::json!({
                "type": "branch_summary",
                "id": "s1",
                "summary": "kept"
            })),
        };
        let payload = to_payload(&full)?;
        assert_eq!(payload["editorText"], "draft");
        assert_eq!(payload["summaryEntry"]["type"], "branch_summary");
        assert_eq!(from_payload::<SessionNavigateTreeResponse>(&payload)?, full);

        let bare = SessionNavigateTreeResponse {
            cancelled: false,
            editor_text: None,
            aborted: None,
            summary_entry: None,
        };
        let payload = to_payload(&bare)?;
        assert_eq!(payload, serde_json::json!({"cancelled": false}));
        assert_eq!(from_payload::<SessionNavigateTreeResponse>(&payload)?, bare);

        let fork = SessionForkRequest {
            entry_id: "e1".to_owned(),
            position: Some(SessionForkPosition::At),
        };
        let payload = to_payload(&fork)?;
        assert_eq!(payload["entryId"], "e1");
        assert_eq!(payload["position"], "at");
        assert_eq!(from_payload::<SessionForkRequest>(&payload)?, fork);

        let ready = SessionReplacementReadyEvent {
            token: "tok-1".to_owned(),
        };
        let payload = to_payload(&ready)?;
        assert_eq!(payload["token"], "tok-1");
        assert_eq!(
            from_payload::<SessionReplacementReadyEvent>(&payload)?,
            ready
        );
        Ok(())
    }

    /// Witness manifest (XC-2, ARC11): the (method, kind) + lifecycle +
    /// payload-digest lockstep consumed by name — parity does not create a
    /// second check. All rules live in [`verify_witness`]; the lockstep test
    /// asserts the identity holds and the mutation tests prove each rule can
    /// actually fail. Mirrors the TypeScript `witness-check.ts` verifier.
    const WITNESS_MANIFEST: &str =
        include_str!("../../../packages/pi-tui-protocol/tests/fixtures/witness-manifest.json");

    /// Pure violation list for the fixture/manifest lockstep. Empty means the
    /// artifacts agree on line count, (method, kind) bijection, ordered
    /// lifecycle discriminants, modifier-combo key events, and the payload
    /// digest. Never reads files and never mutates its inputs, so the
    /// mutation tests can drive it directly.
    #[expect(
        clippy::too_many_lines,
        reason = "witness verifier is one cohesive lockstep check; splitting would obscure rule ordering"
    )]
    fn verify_witness(fixtures: &str, manifest: &serde_json::Value) -> Vec<String> {
        use sha2::Digest;
        use std::collections::HashSet;
        use std::fmt::Write;
        let mut violations = Vec::new();

        let mut count = 0usize;
        let mut seen: HashSet<(String, FrameKind)> = HashSet::new();
        let mut key_events: Vec<serde_json::Value> = Vec::new();
        let mut observed_lifecycle: Vec<String> = Vec::new();
        let manifest_lifecycle: Vec<String> = manifest["lifecycleDiscriminants"]
            .as_array()
            .map(|names| {
                names
                    .iter()
                    .filter_map(|name| name.as_str().map(str::to_owned))
                    .collect::<Vec<String>>()
            })
            .unwrap_or_default();
        for line in fixtures.lines() {
            if line.trim().is_empty() || line.trim_start().starts_with('#') {
                continue;
            }
            // totalLines counts non-blank, non-comment lines whether or not
            // they decode — the TypeScript verifier defines it identically.
            count += 1;
            let Ok(frame) = decode_frame_str(line) else {
                violations.push(format!("fixture line is not a decodable frame: {line}"));
                continue;
            };
            seen.insert((frame.method.clone(), frame.kind));

            if frame.method == Method::UiEvent.as_str()
                && frame.kind == FrameKind::Req
                && let Some(event) = frame.payload.get("event")
                && event.get("type").and_then(|v| v.as_str()) == Some("key")
            {
                let code = event.get("code").and_then(|v| v.as_str()).unwrap_or("");
                let modifiers = event
                    .get("modifiers")
                    .cloned()
                    .unwrap_or(serde_json::json!({}));
                let kind = event
                    .get("kind")
                    .and_then(|v| v.as_str())
                    .unwrap_or("press");
                key_events.push(serde_json::json!({
                    "code": code,
                    "modifiers": modifiers,
                    "kind": kind,
                }));
            }

            // Lifecycle witness frames carry payload.type == method (fixture
            // convention) AND name a manifest-declared discriminant: hooks
            // share the open-method namespace with dialogs and gap surfaces
            // like message_update_delta also carry a type field.
            if frame.kind == FrameKind::Req
                && manifest_lifecycle.iter().any(|name| name == &frame.method)
                && frame.payload.get("type").and_then(|v| v.as_str()) == Some(frame.method.as_str())
            {
                observed_lifecycle.push(frame.method.clone());
            }
        }

        let declared_total = manifest["totalLines"].as_u64().unwrap_or(0);
        if count != usize::try_from(declared_total).unwrap_or(0) {
            violations.push(format!(
                "totalLines mismatch: fixture has {count}, manifest declares {declared_total}"
            ));
        }

        let pair_of = |entry: &serde_json::Value| -> Option<(String, FrameKind)> {
            let method = entry[0].as_str()?;
            let kind = match entry[1].as_str()? {
                "req" => FrameKind::Req,
                "res" => FrameKind::Res,
                "event" => FrameKind::Event,
                "error" => FrameKind::Error,
                _ => return None,
            };
            Some((method.to_owned(), kind))
        };
        let declared_pairs = manifest["methodKindPairs"].as_array();
        if declared_pairs.is_none() {
            violations.push("methodKindPairs missing from manifest".to_owned());
        }
        let mut expected_pairs: Vec<(String, FrameKind)> = Vec::new();
        for entry in declared_pairs.into_iter().flatten() {
            match pair_of(entry) {
                Some(pair) => expected_pairs.push(pair),
                None => violations.push(format!("manifest pair entry is malformed: {entry}")),
            }
        }
        for (method, kind) in &expected_pairs {
            if !seen.contains(&(method.clone(), *kind)) {
                violations.push(format!("missing pair {method}:{}", kind_str(*kind)));
            }
        }
        for (method, kind) in &seen {
            if !expected_pairs.contains(&(method.clone(), *kind)) {
                violations.push(format!(
                    "untracked pair not in manifest: {method}:{}",
                    kind_str(*kind)
                ));
            }
        }
        // manifest_lifecycle (parsed above the frame loop) is the expected
        // ordering; observed_lifecycle was collected during the loop.
        let expected_lifecycle = &manifest_lifecycle;
        let lifecycle_len = observed_lifecycle.len().max(expected_lifecycle.len());
        for index in 0..lifecycle_len {
            let observed = observed_lifecycle.get(index).map(String::as_str);
            let expected = expected_lifecycle.get(index).map(String::as_str);
            if observed != expected {
                violations.push(format!(
                    "lifecycle discriminant mismatch at index {index}: fixture has {}, manifest has {}",
                    observed.unwrap_or("<missing>"),
                    expected.unwrap_or("<missing>")
                ));
                break;
            }
        }

        let expected_key_events = manifest["modifierComboKeyEvents"].as_array();
        let expected_len = expected_key_events.map_or(0, Vec::len);
        if key_events.len() != expected_len {
            violations.push(format!(
                "modifierComboKeyEvents length mismatch: fixture has {}, manifest declares {expected_len}",
                key_events.len()
            ));
        } else if let Some(expected) = expected_key_events {
            for (index, (actual, expected)) in key_events.iter().zip(expected.iter()).enumerate() {
                if actual != expected {
                    violations.push(format!("modifierComboKeyEvents mismatch at index {index}"));
                    break;
                }
            }
        }

        // Payload-byte pin: rejects any single flipped byte even when every
        // envelope rule above still holds.
        let digest = sha2::Sha256::digest(fixtures.as_bytes());
        let mut digest_hex = String::with_capacity(digest.len() * 2);
        for b in &digest {
            let _ = write!(digest_hex, "{b:02x}");
        }
        if let Some(declared) = manifest["fixtureSha256"].as_str() {
            if digest_hex != declared {
                violations.push(format!(
                    "fixtureSha256 mismatch: fixture hashes to {digest_hex}, manifest declares {declared}"
                ));
            }
        } else {
            violations.push("fixtureSha256 missing from manifest".to_owned());
        }

        violations
    }

    fn kind_str(kind: FrameKind) -> &'static str {
        match kind {
            FrameKind::Req => "req",
            FrameKind::Res => "res",
            FrameKind::Event => "event",
            FrameKind::Error => "error",
        }
    }

    #[test]
    fn witness_manifest_lockstep() -> TestResult {
        let manifest: serde_json::Value = serde_json::from_str(WITNESS_MANIFEST)?;
        let violations = verify_witness(FIXTURES, &manifest);
        assert!(
            violations.is_empty(),
            "witness lockstep violations: {violations:?}"
        );
        Ok(())
    }

    /// Every declared open-method constant must have a witnessed
    /// (method, kind) pair. References the constants BY SYMBOL so the
    /// fixture can never drift from the Rust spelling.
    #[test]
    fn witness_covers_every_open_method_constant() -> TestResult {
        let manifest: serde_json::Value = serde_json::from_str(WITNESS_MANIFEST)?;
        let covered: Vec<String> = manifest["methodKindPairs"]
            .as_array()
            .map(|pairs| {
                pairs
                    .iter()
                    .filter_map(|pair| pair[0].as_str().map(str::to_owned))
                    .collect()
            })
            .unwrap_or_default();
        for method in [
            crate::server::EXTENSIONS_LOAD_METHOD,
            crate::server::COMMAND_EXECUTE_METHOD,
            crate::server::TOOL_RENDER_HTML_METHOD,
            crate::server::MESSAGE_UPDATE_DELTA_METHOD,
            methods::TOOL_EXECUTE,
            methods::TOOL_PREPARE,
            methods::TOOL_VALIDATE,
            methods::TOOL_CANCEL,
            methods::PROVIDER_STREAM,
            methods::PROVIDER_CANCEL,
        ] {
            assert!(
                covered.iter().any(|m| m == method),
                "open method constant {method} has no witnessed fixture pair"
            );
        }
        Ok(())
    }

    /// The new fixture surfaces decode into their real Rust types: the
    /// registry snapshot carries every section, streaming updates and
    /// provider events decode correlated, and the error variant carries the
    /// non-retryable `ErrorPayload` shape.
    #[test]
    fn witness_gap_surfaces_decode_typed() -> TestResult {
        let mut snapshot: Option<RegistrySnapshot> = None;
        let mut tool_update: Option<ToolUpdate> = None;
        let mut provider_event: Option<ProviderEvent> = None;
        let mut delta_error: Option<ErrorPayload> = None;
        let mut correlated_update_ids: Vec<FrameId> = Vec::new();

        for line in FIXTURES.lines() {
            if line.trim().is_empty() || line.trim_start().starts_with('#') {
                continue;
            }
            let frame = decode_frame_str(line)?;
            if frame.kind == FrameKind::Res && frame.method == crate::server::EXTENSIONS_LOAD_METHOD
            {
                snapshot = Some(from_payload::<RegistrySnapshot>(&frame.payload)?);
            } else if frame.kind == FrameKind::Event && frame.method == Method::ToolUpdate.as_str()
            {
                tool_update = Some(from_payload::<ToolUpdate>(&frame.payload)?);
                if frame.id != 0 {
                    correlated_update_ids.push(frame.id);
                }
            } else if frame.kind == FrameKind::Event && frame.method == "providerEvent" {
                provider_event = Some(from_payload::<ProviderEvent>(&frame.payload)?);
            } else if frame.kind == FrameKind::Error
                && frame.payload.get("code").and_then(|v| v.as_str()) == Some("timeout")
            {
                delta_error = Some(from_payload::<ErrorPayload>(&frame.payload)?);
            }
        }

        let snapshot =
            snapshot.ok_or("witness has no extensions.load RegistrySnapshot response")?;
        assert!(!snapshot.tools.is_empty(), "snapshot must witness tools");
        assert!(
            !snapshot.commands.is_empty(),
            "snapshot must witness commands"
        );
        assert!(
            !snapshot.providers.is_empty(),
            "snapshot must witness providers"
        );
        assert!(
            !snapshot.handlers.is_empty(),
            "snapshot must witness lifecycle handlers"
        );
        assert!(
            snapshot.terminal_input,
            "snapshot must witness terminalInput"
        );
        assert!(
            tool_update.is_some(),
            "witness has no toolUpdate event frame"
        );
        assert!(
            provider_event.is_some(),
            "witness has no providerEvent frame"
        );
        assert!(
            !correlated_update_ids.is_empty(),
            "witness never exercises correlated (non-zero id) streaming updates"
        );
        let error = delta_error.ok_or("witness has no message_update_delta error frame")?;
        assert_eq!(error.code, "timeout");
        assert!(!error.retryable);
        Ok(())
    }

    #[test]
    #[expect(
        clippy::unwrap_used,
        reason = "WITNESS_MANIFEST is include_str! of a committed JSON fixture; parse failure is a build-time contract violation"
    )]
    fn witness_mutation_m1_payload_byte_is_rejected() {
        let manifest: serde_json::Value = serde_json::from_str(WITNESS_MANIFEST).unwrap();
        let mutated = FIXTURES.replacen("\"title\"", "\"titel\"", 1);
        assert_ne!(mutated, FIXTURES);
        let violations = verify_witness(&mutated, &manifest);
        assert!(
            violations
                .iter()
                .any(|v| v.starts_with("fixtureSha256 mismatch")),
            "payload byte flip must be rejected by the digest rule, got {violations:?}"
        );
    }

    #[test]
    #[expect(
        clippy::unwrap_used,
        reason = "WITNESS_MANIFEST is include_str! of a committed JSON fixture; parse failure is a build-time contract violation"
    )]
    #[expect(
        clippy::expect_used,
        reason = "tool.cancel fixture line is committed in include_str! bytes; absence is a build-time contract violation"
    )]
    fn witness_mutation_m2_dropped_line_is_rejected() {
        let manifest: serde_json::Value = serde_json::from_str(WITNESS_MANIFEST).unwrap();
        let mut lines: Vec<&str> = FIXTURES
            .lines()
            .filter(|line| !line.trim().is_empty() && !line.trim_start().starts_with('#'))
            .collect();
        let index = lines
            .iter()
            .position(|line| line.contains("\"method\":\"tool.cancel\""))
            .expect("tool.cancel fixture line");
        lines.remove(index);
        let mutated = lines.join("\n");
        let violations = verify_witness(&mutated, &manifest);
        assert!(
            violations
                .iter()
                .any(|v| v.starts_with("totalLines mismatch")),
            "dropped line must break totalLines, got {violations:?}"
        );
        assert!(
            violations.iter().any(|v| v.starts_with("missing pair")),
            "dropped line must break a pair, got {violations:?}"
        );
    }

    #[test]
    #[expect(
        clippy::unwrap_used,
        reason = "WITNESS_MANIFEST is include_str! of a committed JSON fixture; parse failure is a build-time contract violation"
    )]
    #[expect(
        clippy::expect_used,
        reason = "lifecycleDiscriminants array is committed in WITNESS_MANIFEST; absence is a build-time contract violation"
    )]
    fn witness_mutation_m3_lifecycle_swap_is_rejected_at_named_index() {
        let mut manifest: serde_json::Value = serde_json::from_str(WITNESS_MANIFEST).unwrap();
        let mut names = manifest["lifecycleDiscriminants"]
            .as_array()
            .cloned()
            .expect("lifecycleDiscriminants");
        names.swap(0, 1);
        manifest["lifecycleDiscriminants"] = serde_json::Value::Array(names);
        let violations = verify_witness(FIXTURES, &manifest);
        assert!(
            violations
                .iter()
                .any(|v| v.contains("lifecycle discriminant mismatch at index 0")),
            "swapped discriminants must fail at a named index, got {violations:?}"
        );
    }

    #[test]
    #[expect(
        clippy::unwrap_used,
        reason = "WITNESS_MANIFEST is include_str! of a committed JSON fixture; parse failure is a build-time contract violation"
    )]
    fn witness_mutation_m5_untracked_frame_is_rejected_by_name() {
        let manifest: serde_json::Value = serde_json::from_str(WITNESS_MANIFEST).unwrap();
        let mutated = format!(
            "{FIXTURES}\n{{\"id\":900,\"kind\":\"req\",\"method\":\"who.is\",\"payload\":{{}}}}\n"
        );
        let violations = verify_witness(&mutated, &manifest);
        assert!(
            violations
                .iter()
                .any(|v| v.starts_with("untracked pair not in manifest")),
            "untracked frame must be rejected by name, got {violations:?}"
        );
    }

    #[test]
    #[expect(
        clippy::unwrap_used,
        reason = "WITNESS_MANIFEST is include_str! of a committed JSON fixture; parse failure is a build-time contract violation"
    )]
    #[expect(
        clippy::expect_used,
        reason = "fixture lines are committed in include_str! bytes; absence is a build-time contract violation"
    )]
    fn witness_mutation_m6_duplicated_line_is_rejected() {
        let manifest: serde_json::Value = serde_json::from_str(WITNESS_MANIFEST).unwrap();
        let lines: Vec<&str> = FIXTURES
            .lines()
            .filter(|line| !line.trim().is_empty() && !line.trim_start().starts_with('#'))
            .collect();
        let mut duplicated = lines.clone();
        duplicated.push(lines.last().copied().expect("fixture line"));
        let mutated = duplicated.join("\n");
        let violations = verify_witness(&mutated, &manifest);
        assert!(
            violations
                .iter()
                .any(|v| v.starts_with("totalLines mismatch")),
            "duplicated line must break totalLines, got {violations:?}"
        );
    }
}
