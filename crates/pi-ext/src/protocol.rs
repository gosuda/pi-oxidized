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
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
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
        let payload = to_payload(&flags)?;
        assert_eq!(from_payload::<FlagsSetRequest>(&payload)?, flags);

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
}
