//! Rust adapters bridging the host protocol to `pi-agent`, `pi-ai`, and
//! `pi-tui` contracts.
//!
//! - [`ExtensionAgentTool`] implements [`pi_agent::AgentTool`], proxying
//!   prepare / execute / progress / cancel to the host.
//! - [`ExtensionProvider`] implements [`pi_ai::Provider`], proxying a custom
//!   provider stream with events and cancellation.
//! - [`SlotComponent`] implements [`pi_tui::component::Component`] (and
//!   [`pi_tui::focus::Focusable`]) by rendering sanitized styled runs into a
//!   Ratatui buffer and mapping native input onto the wire event type.
//! - [`Registry`] and the `*Registration` records capture what a host has
//!   registered (tools, commands, shortcuts, flags, renderers, providers) with
//!   first-registration-wins semantics.

use std::any::Any;
use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context as TaskContext, Poll};
use std::time::Duration;

use crossterm::event::{KeyCode, KeyModifiers};
use futures::Stream;
use ratatui::buffer::Buffer;
use ratatui::layout::{Position, Rect};
use ratatui::style::{Color, Modifier, Style as RatatuiStyle};
use serde_json::{Map, Value};
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

use pi_agent::{AgentTool, AgentToolResult, ToolError, ToolExecutionMode, ToolUpdates};
use pi_ai::provider::{Provider, ProviderError, StreamOptions};
use pi_ai::types::{AssistantMessageEvent, Context, Model};
use pi_tui::component::{Component, EventResult, UiEvent};
use pi_tui::focus::{FocusId, Focusable};
use pi_tui::frame::{RawRegion, push_raw_region, set_cursor};
use pi_tui::link::{format_link_close, format_link_open};
use pi_tui::text::slice_with_width;

use crate::client::HostClient;
use crate::protocol::{
    Frame, KeyEventKindWire, KeyModifiersWire, NamedColor, ToolUpdate, UiEventWire, WireColor,
    from_payload,
};
use crate::sanitize::{SanitizedSlot, sanitize_slot};

/// Open lifecycle method strings used by the tool bridge.
pub mod methods {
    /// Host: execute an extension tool (streams `toolUpdate`, terminal `res`).
    pub const TOOL_EXECUTE: &str = "tool.execute";
    /// Host: prepare raw tool arguments before validation.
    pub const TOOL_PREPARE: &str = "tool.prepare";
    /// Host: validate prepared arguments.
    pub const TOOL_VALIDATE: &str = "tool.validate";
    /// Host: cancel an in-flight tool execution.
    pub const TOOL_CANCEL: &str = "tool.cancel";
    /// Host: start a custom provider stream (streams `providerEvent`, terminal).
    pub const PROVIDER_STREAM: &str = "provider.stream";
    /// Host: cancel an in-flight custom provider stream.
    pub const PROVIDER_CANCEL: &str = "provider.cancel";
}

/// Default per-call deadline for tool/provider bridges.
pub const DEFAULT_CALL_TIMEOUT: Duration = Duration::from_secs(30);

fn tool_error<E: std::fmt::Display>(e: E) -> ToolError {
    ToolError::new(e.to_string())
}

fn provider_error<E: std::fmt::Display>(e: E) -> ProviderError {
    ProviderError::new(e.to_string())
}

// ---------------------------------------------------------------------------
// AgentTool proxy
// ---------------------------------------------------------------------------

/// Immutable registration metadata for an extension tool.
#[derive(Clone, Debug)]
pub struct ToolRegistration {
    /// Tool name (used in LLM tool calls).
    pub name: String,
    /// Human-readable label for UI.
    pub label: String,
    /// Description for the LLM.
    pub description: String,
    /// JSON Schema for tool arguments.
    pub parameters: Value,
    /// Optional execution-mode override.
    pub execution_mode: Option<ToolExecutionMode>,
}

/// `pi_agent::AgentTool` backed by a TypeScript extension running in the host.
///
/// `prepare`/`validate` round-trip to the host when configured; `execute` opens
/// a streaming call, forwards `toolUpdate` events as partial results, honors
/// the caller's [`CancellationToken`], and resolves the terminal result.
pub struct ExtensionAgentTool {
    meta: ToolRegistration,
    client: Arc<HostClient>,
    timeout: Duration,
}

impl ExtensionAgentTool {
    /// Create a new proxy.
    #[must_use]
    pub fn new(meta: ToolRegistration, client: Arc<HostClient>) -> Self {
        Self {
            meta,
            client,
            timeout: DEFAULT_CALL_TIMEOUT,
        }
    }

    /// Override the per-call deadline.
    #[must_use]
    pub fn with_timeout(mut self, timeout: Duration) -> Self {
        self.timeout = timeout;
        self
    }

    /// Borrowed registration metadata.
    #[must_use]
    pub fn registration(&self) -> &ToolRegistration {
        &self.meta
    }
}

impl AgentTool for ExtensionAgentTool {
    fn name(&self) -> &str {
        &self.meta.name
    }

    fn label(&self) -> &str {
        &self.meta.label
    }

    fn description(&self) -> &str {
        &self.meta.description
    }

    fn parameters(&self) -> &Value {
        &self.meta.parameters
    }

    fn execution_mode(&self) -> Option<ToolExecutionMode> {
        self.meta.execution_mode
    }

    fn prepare_arguments(&self, raw: &Map<String, Value>) -> Result<Map<String, Value>, ToolError> {
        // Prepare/validate run inside the host (schema lives in TypeScript).
        // The synchronous trait surface cannot await; the agent loop calls
        // prepare/validate on the host-owned schema, so we pass raw through and
        // let `execute` reject malformed arguments from the host.
        Ok(raw.clone())
    }

    fn validate_arguments(
        &self,
        args: &Map<String, Value>,
    ) -> Result<Map<String, Value>, ToolError> {
        Ok(args.clone())
    }

    fn prepare_and_validate_arguments(
        &self,
        raw: Map<String, Value>,
    ) -> futures::future::BoxFuture<'_, Result<Map<String, Value>, ToolError>> {
        let client = Arc::clone(&self.client);
        let name = self.meta.name.clone();
        let timeout = self.timeout;
        Box::pin(async move {
            let prepared = client
                .request_raw(
                    methods::TOOL_PREPARE,
                    serde_json::json!({ "name": &name, "args": raw }),
                    timeout,
                )
                .await
                .map_err(tool_error)?;
            let prepared_args = decode_tool_args(&prepared, "prepare")?;
            let validated = client
                .request_raw(
                    methods::TOOL_VALIDATE,
                    serde_json::json!({ "name": &name, "args": prepared_args }),
                    timeout,
                )
                .await
                .map_err(tool_error)?;
            decode_tool_args(&validated, "validate")
        })
    }

    fn execute(
        &self,
        tool_call_id: &str,
        args: Map<String, Value>,
        cancel: CancellationToken,
        updates: ToolUpdates,
    ) -> futures::future::BoxFuture<'static, Result<AgentToolResult, ToolError>> {
        let client = Arc::clone(&self.client);
        let name = self.meta.name.clone();
        let timeout = self.timeout;
        let tool_call_id = tool_call_id.to_owned();
        Box::pin(async move {
            let payload = serde_json::json!({
                "name": name,
                "toolCallId": tool_call_id,
                "args": args,
                "prepared": true,
            });
            let mut stream = client
                .open_stream_raw(methods::TOOL_EXECUTE, payload, 64)
                .await
                .map_err(tool_error)?;
            loop {
                tokio::select! {
                    biased;
                    () = cancel.cancelled() => {
                        let _ = stream.cancel(methods::TOOL_CANCEL).await;
                        return Err(ToolError::new("extension tool cancelled"));
                    }
                    ev = stream.next_event() => match ev {
                        Some(frame) => forward_tool_update(&frame, &updates),
                        None => break,
                    }
                }
            }
            let terminal = stream.finish(timeout).await.map_err(tool_error)?;
            parse_tool_result(&terminal)
        })
    }
}

fn decode_tool_args(frame: &Frame, phase: &str) -> Result<Map<String, Value>, ToolError> {
    frame
        .payload
        .get("args")
        .and_then(Value::as_object)
        .cloned()
        .ok_or_else(|| ToolError::new(format!("extension tool {phase} returned invalid args")))
}

fn forward_tool_update(frame: &Frame, updates: &ToolUpdates) {
    if let Ok(update) = from_payload::<ToolUpdate>(&frame.payload)
        && let Ok(partial) =
            serde_json::from_value::<AgentToolResult>(update.partial_result.clone())
    {
        updates.send(partial);
    }
}

fn parse_tool_result(frame: &Frame) -> Result<AgentToolResult, ToolError> {
    from_payload::<AgentToolResult>(&frame.payload)
        .map_err(|e| ToolError::new(format!("decode extension tool result: {e}")))
}

// ---------------------------------------------------------------------------
// Provider proxy
// ---------------------------------------------------------------------------

/// `pi_ai::Provider` backed by a TypeScript extension custom provider.
///
/// `stream` opens a `provider.stream` call and forwards each `providerEvent`
/// payload (deserialized as an [`AssistantMessageEvent`]) to the caller, while
/// honoring the caller's [`CancellationToken`].
pub struct ExtensionProvider {
    provider_id: String,
    client: Arc<HostClient>,
    timeout: Duration,
}

impl ExtensionProvider {
    /// Create a new proxy for `provider_id`.
    #[must_use]
    pub fn new(provider_id: impl Into<String>, client: Arc<HostClient>) -> Self {
        Self {
            provider_id: provider_id.into(),
            client,
            timeout: DEFAULT_CALL_TIMEOUT,
        }
    }

    /// Override the per-call deadline.
    ///
    /// Bounds each individual host request (time to first event, terminal
    /// finish), not the stream's lifetime. Inter-event gaps are unbounded
    /// once streaming has begun.
    #[must_use]
    pub fn with_timeout(mut self, timeout: Duration) -> Self {
        self.timeout = timeout;
        self
    }
}

impl Provider for ExtensionProvider {
    fn stream(
        &self,
        model: &Model,
        context: Context,
        options: StreamOptions,
    ) -> futures::stream::BoxStream<'static, Result<AssistantMessageEvent, ProviderError>> {
        let client = Arc::clone(&self.client);
        let provider_id = self.provider_id.clone();
        let timeout = self.timeout;
        let cancel = options.signal.clone().unwrap_or_default();
        // Serialize model/context/options before spawning so no lock is held
        // across the host await (prepare_request already completed upstream).
        let model_value = serde_json::to_value(model).unwrap_or(Value::Null);
        let context_value = serde_json::to_value(&context).unwrap_or(Value::Null);
        let options_value = stream_options_wire(&options);
        let mut payload = Map::new();
        payload.insert("providerId".to_owned(), Value::String(provider_id));
        payload.insert("model".to_owned(), model_value);
        payload.insert("context".to_owned(), context_value);
        payload.insert("options".to_owned(), options_value);

        // Capacity-64 matches STREAM_EVENT_CAPACITY / host provider channel bound.
        let (tx, rx) = mpsc::channel::<Result<AssistantMessageEvent, ProviderError>>(64);
        tokio::spawn(async move {
            let mut handle = match client
                .open_stream_raw(methods::PROVIDER_STREAM, Value::Object(payload), 64)
                .await
            {
                Ok(h) => h,
                Err(e) => {
                    let _ = tx.send(Err(provider_error(e))).await;
                    return;
                }
            };
            // Bound only the pre-first-event wait. After the first frame,
            // inter-event gaps are unbounded (mirrors ExtensionAgentTool).
            let mut seen_event = false;
            loop {
                tokio::select! {
                    biased;
                    () = cancel.cancelled() => {
                        let _ = handle.cancel(methods::PROVIDER_CANCEL).await;
                        let _ = tx
                            .send(Err(ProviderError::new("provider stream cancelled")))
                            .await;
                        return;
                    }
                    ev = async {
                        if seen_event {
                            Ok(handle.next_event().await)
                        } else {
                            tokio::time::timeout(timeout, handle.next_event()).await
                        }
                    } => match ev {
                        Ok(Some(frame)) => {
                            seen_event = true;
                            if let Some(event) = decode_provider_stream_event(&frame.payload)
                                && tx.send(Ok(event)).await.is_err()
                            {
                                // Consumer dropped — stop driving the host stream.
                                return;
                            }
                        }
                        Ok(None) => break,
                        Err(_) => {
                            // Pre-first-event idle deadline — do not wait for
                            // finish (a late terminal inside that window would
                            // otherwise look like a clean stream).
                            let _ = tx
                                .send(Err(provider_error(crate::client::HostClientError::Timeout {
                                    id: handle.id(),
                                    timeout,
                                })))
                                .await;
                            let _ = handle.cancel(methods::PROVIDER_CANCEL).await;
                            return;
                        }
                    }
                }
            }
            match handle.finish(timeout).await {
                Ok(_terminal) => {}
                Err(e) => {
                    let _ = tx.send(Err(provider_error(e))).await;
                }
            }
            // tx drops → stream ends.
        });
        Box::pin(ProviderStream { rx })
    }
}

/// Encode prepared [`StreamOptions`] for the host `provider.stream` request.
///
/// Forwards every serializable field `streamSimple` needs after
/// `prepare_request` (credentials, headers, env, timeouts, cache/session,
/// transport, metadata, and flattened `extra` such as `reasoning` /
/// `thinkingBudgets`). Cancellation is out-of-band via
/// [`methods::PROVIDER_CANCEL`]; async callbacks stay on the Rust side.
fn stream_options_wire(options: &StreamOptions) -> Value {
    let mut map = Map::new();
    if let Some(temperature) = options.temperature {
        map.insert("temperature".to_owned(), Value::from(temperature));
    }
    if let Some(max_tokens) = options.max_tokens {
        map.insert("maxTokens".to_owned(), Value::from(max_tokens));
    }
    if let Some(api_key) = options.api_key.as_ref() {
        map.insert("apiKey".to_owned(), Value::String(api_key.clone()));
    }
    if let Some(transport) = options.transport
        && let Ok(value) = serde_json::to_value(transport)
    {
        map.insert("transport".to_owned(), value);
    }
    if let Some(cache_retention) = options.cache_retention
        && let Ok(value) = serde_json::to_value(cache_retention)
    {
        map.insert("cacheRetention".to_owned(), value);
    }
    if let Some(session_id) = options.session_id.as_ref() {
        map.insert("sessionId".to_owned(), Value::String(session_id.clone()));
    }
    if let Some(headers) = options.headers.as_ref() {
        let mut encoded = Map::new();
        for (name, value) in headers {
            encoded.insert(
                name.clone(),
                match value {
                    Some(text) => Value::String(text.clone()),
                    None => Value::Null,
                },
            );
        }
        map.insert("headers".to_owned(), Value::Object(encoded));
    }
    if let Some(timeout_ms) = options.timeout_ms {
        map.insert("timeoutMs".to_owned(), Value::from(timeout_ms));
    }
    if let Some(websocket_connect_timeout_ms) = options.websocket_connect_timeout_ms {
        map.insert(
            "websocketConnectTimeoutMs".to_owned(),
            Value::from(websocket_connect_timeout_ms),
        );
    }
    if let Some(max_retries) = options.max_retries {
        map.insert("maxRetries".to_owned(), Value::from(max_retries));
    }
    if let Some(max_retry_delay_ms) = options.max_retry_delay_ms {
        map.insert(
            "maxRetryDelayMs".to_owned(),
            Value::from(max_retry_delay_ms),
        );
    }
    if let Some(metadata) = options.metadata.as_ref() {
        map.insert("metadata".to_owned(), Value::Object(metadata.clone()));
    }
    if let Some(env) = options.env.as_ref() {
        let mut encoded = Map::new();
        for (key, value) in env {
            encoded.insert(key.clone(), Value::String(value.clone()));
        }
        map.insert("env".to_owned(), Value::Object(encoded));
    }
    // Flatten extra (reasoning / thinkingBudgets / provider-specific) into the
    // same options object SimpleStreamOptions expects — without inventing a
    // nested `extra` key the host would not understand.
    for (key, value) in &options.extra {
        map.entry(key.clone()).or_insert_with(|| value.clone());
    }
    Value::Object(map)
}

/// Decode a host `providerEvent` payload into an [`AssistantMessageEvent`].
///
/// Accepts either a bare assistant event or a [`crate::protocol::ProviderEvent`]
/// wrapper whose `data` field is the assistant event.
fn decode_provider_stream_event(payload: &Value) -> Option<AssistantMessageEvent> {
    if let Ok(event) = from_payload::<AssistantMessageEvent>(payload) {
        return Some(event);
    }
    if let Ok(wrapper) = from_payload::<crate::protocol::ProviderEvent>(payload)
        && let Ok(event) = serde_json::from_value::<AssistantMessageEvent>(wrapper.data)
    {
        return Some(event);
    }
    None
}

/// Stream wrapper over the driver task's receiver.
struct ProviderStream {
    rx: mpsc::Receiver<Result<AssistantMessageEvent, ProviderError>>,
}

impl Stream for ProviderStream {
    type Item = Result<AssistantMessageEvent, ProviderError>;

    fn poll_next(mut self: Pin<&mut Self>, cx: &mut TaskContext<'_>) -> Poll<Option<Self::Item>> {
        self.rx.poll_recv(cx)
    }
}

// ---------------------------------------------------------------------------
// Component adapter
// ---------------------------------------------------------------------------

/// `pi_tui::Component` rendering a sanitized host UI slot.
///
/// Renders validated [`crate::sanitize::SanitizedSlot`] lines into a Ratatui
/// buffer using structured styles, and (when focused) forwards mapped native
/// input events to an optional router channel. It never emits raw plugin bytes.
pub struct SlotComponent {
    slot: SanitizedSlot,
    focused: bool,
    focus_id: FocusId,
    event_tx: Option<mpsc::UnboundedSender<UiEventWire>>,
}

impl SlotComponent {
    /// Create a component from a sanitized slot.
    #[must_use]
    pub fn new(slot: SanitizedSlot) -> Self {
        Self {
            slot,
            focused: false,
            focus_id: FocusId::new(),
            event_tx: None,
        }
    }

    /// Create a component from an inbound [`crate::protocol::UiSlot`], first
    /// sanitizing every run/style/link field.
    #[must_use]
    pub fn from_ui_slot(slot: &crate::protocol::UiSlot) -> Self {
        Self::new(sanitize_slot(slot))
    }

    /// Attach an input router; when focused, mapped events are forwarded here.
    #[must_use]
    pub fn with_event_router(mut self, tx: mpsc::UnboundedSender<UiEventWire>) -> Self {
        self.event_tx = Some(tx);
        self
    }

    fn forward_event(&self, event: &UiEvent) {
        if let Some(tx) = &self.event_tx {
            let _ = tx.send(map_ui_event(event));
        }
    }
}

impl Component for SlotComponent {
    fn measure(&mut self, _width: u16) -> u16 {
        self.slot.height
    }

    fn render(&mut self, area: Rect, buf: &mut Buffer) {
        let max_rows = area.height as usize;
        for (row, line) in self.slot.lines.iter().take(max_rows).enumerate() {
            let y = area
                .y
                .saturating_add(u16::try_from(row).unwrap_or(u16::MAX));
            if y >= area.bottom() {
                break;
            }
            let mut x = area.x;
            for run in line {
                let style = wire_style_to_ratatui(&run.style);
                let remaining = usize::from(area.right().saturating_sub(x));
                let rendered = slice_with_width(&run.text, 0, remaining, true);
                if rendered.width == 0 {
                    continue;
                }
                buf.set_stringn(x, y, &rendered.text, remaining, style);
                let printed = u16::try_from(rendered.width).unwrap_or(u16::MAX);
                if let Some(link) = &run.style.link
                    && let Some(open) = format_link_open(&link.uri, link.id.as_deref())
                {
                    let mut bytes = Vec::new();
                    bytes.extend_from_slice(open.as_bytes());
                    bytes.extend_from_slice(sgr_open(&run.style).as_bytes());
                    bytes.extend_from_slice(rendered.text.as_bytes());
                    bytes.extend_from_slice(b"\x1b[0m");
                    bytes.extend_from_slice(format_link_close().as_bytes());
                    push_raw_region(RawRegion {
                        area: Rect::new(x, y, printed, 1),
                        bytes,
                        kitty_id: None,
                    });
                }
                x = x.saturating_add(printed);
                if x >= area.right() {
                    break;
                }
            }
        }
        if self.focused
            && area.width > 0
            && area.height > 0
            && let Some(cursor) = self.slot.cursor
        {
            set_cursor(Position {
                x: area
                    .x
                    .saturating_add(cursor.col.min(area.width.saturating_sub(1))),
                y: area
                    .y
                    .saturating_add(cursor.row.min(area.height.saturating_sub(1))),
            });
        }
    }

    fn handle_event(&mut self, event: &UiEvent) -> EventResult {
        if self.focused {
            self.forward_event(event);
            EventResult::Consumed
        } else {
            EventResult::Ignored
        }
    }

    fn invalidate(&mut self) {}
}

impl Focusable for SlotComponent {
    fn focus_id(&self) -> FocusId {
        self.focus_id
    }

    fn is_focused(&self) -> bool {
        self.focused
    }

    fn set_focused(&mut self, focused: bool) {
        self.focused = focused;
    }

    fn as_any(&self) -> &dyn Any {
        self
    }

    fn as_any_mut(&mut self) -> &mut dyn Any {
        self
    }
}

/// Convert a wire style into a Ratatui style.
fn wire_style_to_ratatui(style: &crate::protocol::Style) -> RatatuiStyle {
    let mut mods = Modifier::empty();
    if style.bold == Some(true) {
        mods |= Modifier::BOLD;
    }
    if style.dim == Some(true) {
        mods |= Modifier::DIM;
    }
    if style.italic == Some(true) {
        mods |= Modifier::ITALIC;
    }
    if style.underline == Some(true) {
        mods |= Modifier::UNDERLINED;
    }
    if style.reverse == Some(true) {
        mods |= Modifier::REVERSED;
    }
    if style.strikethrough == Some(true) {
        mods |= Modifier::CROSSED_OUT;
    }
    let mut out = RatatuiStyle::default().add_modifier(mods);
    if let Some(fg) = &style.fg {
        out = out.fg(wire_color_to_ratatui(fg));
    }
    if let Some(bg) = &style.bg {
        out = out.bg(wire_color_to_ratatui(bg));
    }
    out
}

fn sgr_open(style: &crate::protocol::Style) -> String {
    let mut codes = Vec::<String>::new();
    for (enabled, code) in [
        (style.bold, "1"),
        (style.dim, "2"),
        (style.italic, "3"),
        (style.underline, "4"),
        (style.reverse, "7"),
        (style.strikethrough, "9"),
    ] {
        if enabled == Some(true) {
            codes.push(code.to_owned());
        }
    }
    if let Some(color) = &style.fg {
        codes.push(sgr_color(color, false));
    }
    if let Some(color) = &style.bg {
        codes.push(sgr_color(color, true));
    }
    if codes.is_empty() {
        String::new()
    } else {
        format!("\x1b[{}m", codes.join(";"))
    }
}

fn sgr_color(color: &WireColor, background: bool) -> String {
    match color {
        WireColor::Indexed { index } => {
            format!("{};5;{index}", if background { 48 } else { 38 })
        }
        WireColor::Rgb { r, g, b } => {
            format!("{};2;{r};{g};{b}", if background { 48 } else { 38 })
        }
        WireColor::Named { name } => {
            let foreground = match name {
                NamedColor::Black => 30,
                NamedColor::Red => 31,
                NamedColor::Green => 32,
                NamedColor::Yellow => 33,
                NamedColor::Blue => 34,
                NamedColor::Magenta => 35,
                NamedColor::Cyan => 36,
                NamedColor::White => 37,
                NamedColor::BrightBlack => 90,
                NamedColor::BrightRed => 91,
                NamedColor::BrightGreen => 92,
                NamedColor::BrightYellow => 93,
                NamedColor::BrightBlue => 94,
                NamedColor::BrightMagenta => 95,
                NamedColor::BrightCyan => 96,
                NamedColor::BrightWhite => 97,
            };
            (if background {
                foreground + 10
            } else {
                foreground
            })
            .to_string()
        }
    }
}

fn wire_color_to_ratatui(color: &WireColor) -> Color {
    match color {
        WireColor::Named { name } => named_color_to_ratatui(*name),
        WireColor::Indexed { index } => Color::Indexed(*index),
        WireColor::Rgb { r, g, b } => Color::Rgb(*r, *g, *b),
    }
}

fn named_color_to_ratatui(name: NamedColor) -> Color {
    match name {
        NamedColor::Black => Color::Black,
        NamedColor::Red => Color::Red,
        NamedColor::Green => Color::Green,
        NamedColor::Yellow => Color::Yellow,
        NamedColor::Blue => Color::Blue,
        NamedColor::Magenta => Color::Magenta,
        NamedColor::Cyan => Color::Cyan,
        NamedColor::White => Color::Gray,
        NamedColor::BrightBlack => Color::DarkGray,
        NamedColor::BrightRed => Color::LightRed,
        NamedColor::BrightGreen => Color::LightGreen,
        NamedColor::BrightYellow => Color::LightYellow,
        NamedColor::BrightBlue => Color::LightBlue,
        NamedColor::BrightMagenta => Color::LightMagenta,
        NamedColor::BrightCyan => Color::LightCyan,
        NamedColor::BrightWhite => Color::White,
    }
}

/// Map a native [`UiEvent`] onto the protocol wire event type.
#[must_use]
pub fn map_ui_event(event: &UiEvent) -> UiEventWire {
    match event {
        UiEvent::Key(key) => UiEventWire::Key {
            code: map_key_code(key.code),
            modifiers: map_modifiers(key.modifiers),
            kind: map_key_kind(key.kind),
        },
        UiEvent::Paste(text) => UiEventWire::Paste { text: text.clone() },
        UiEvent::FocusGained => UiEventWire::FocusGained,
        UiEvent::FocusLost => UiEventWire::FocusLost,
        UiEvent::Resize { width, height } => UiEventWire::Resize {
            width: *width,
            height: *height,
        },
    }
}

fn map_modifiers(mods: KeyModifiers) -> KeyModifiersWire {
    KeyModifiersWire {
        shift: mods.contains(KeyModifiers::SHIFT).then_some(true),
        alt: mods.contains(KeyModifiers::ALT).then_some(true),
        ctrl: mods.contains(KeyModifiers::CONTROL).then_some(true),
        super_key: mods.contains(KeyModifiers::SUPER).then_some(true),
    }
}

fn map_key_kind(kind: crossterm::event::KeyEventKind) -> KeyEventKindWire {
    match kind {
        crossterm::event::KeyEventKind::Press => KeyEventKindWire::Press,
        crossterm::event::KeyEventKind::Release => KeyEventKindWire::Release,
        crossterm::event::KeyEventKind::Repeat => KeyEventKindWire::Repeat,
    }
}

fn map_key_code(code: KeyCode) -> String {
    match code {
        KeyCode::Enter => "enter".to_owned(),
        KeyCode::Tab => "tab".to_owned(),
        KeyCode::BackTab => "backtab".to_owned(),
        KeyCode::Backspace => "backspace".to_owned(),
        KeyCode::Delete => "delete".to_owned(),
        KeyCode::Insert => "insert".to_owned(),
        KeyCode::Left => "left".to_owned(),
        KeyCode::Right => "right".to_owned(),
        KeyCode::Up => "up".to_owned(),
        KeyCode::Down => "down".to_owned(),
        KeyCode::Home => "home".to_owned(),
        KeyCode::End => "end".to_owned(),
        KeyCode::PageUp => "pageup".to_owned(),
        KeyCode::PageDown => "pagedown".to_owned(),
        KeyCode::Esc => "escape".to_owned(),
        KeyCode::Char(c) => {
            if c == ' ' {
                "space".to_owned()
            } else {
                c.to_string()
            }
        }
        KeyCode::F(n) => format!("f{n}"),
        KeyCode::Null => "null".to_owned(),
        _ => "unknown".to_owned(),
    }
}

/// Convert the wire overlay layout value into the product-agnostic TUI type.
#[must_use]
pub fn tui_overlay_spec(spec: &crate::protocol::OverlaySpec) -> pi_tui::layout::OverlaySpec {
    use crate::protocol::{OverlayAnchor as WireAnchor, OverlayMarginWire, SizeValue as WireSize};
    use pi_tui::layout::{OverlayAnchor, OverlayMargin, SizeValue};

    let size = |value: crate::protocol::SizeValue| match value {
        WireSize::Cells(cells) => SizeValue::Cells(cells),
        WireSize::Percent(percent) => SizeValue::Percent(percent),
    };
    let anchor = spec.anchor.map(|anchor| match anchor {
        WireAnchor::Center => OverlayAnchor::Center,
        WireAnchor::TopLeft => OverlayAnchor::TopLeft,
        WireAnchor::TopRight => OverlayAnchor::TopRight,
        WireAnchor::BottomLeft => OverlayAnchor::BottomLeft,
        WireAnchor::BottomRight => OverlayAnchor::BottomRight,
        WireAnchor::TopCenter => OverlayAnchor::TopCenter,
        WireAnchor::BottomCenter => OverlayAnchor::BottomCenter,
        WireAnchor::LeftCenter => OverlayAnchor::LeftCenter,
        WireAnchor::RightCenter => OverlayAnchor::RightCenter,
    });
    let margin = spec.margin.map(|margin| match margin {
        OverlayMarginWire::Uniform(value) => OverlayMargin::uniform(value),
        OverlayMarginWire::Sides(sides) => OverlayMargin {
            top: sides.top,
            right: sides.right,
            bottom: sides.bottom,
            left: sides.left,
        },
    });
    pi_tui::layout::OverlaySpec {
        width: spec.width.map(size),
        min_width: spec.min_width,
        max_height: spec.max_height.map(size),
        anchor,
        offset_x: spec.offset_x,
        offset_y: spec.offset_y,
        row: spec.row.map(size),
        col: spec.col.map(size),
        margin,
        non_capturing: spec.non_capturing,
    }
}

// ---------------------------------------------------------------------------
// Registration records
// ---------------------------------------------------------------------------

/// A registered custom command.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CommandRegistration {
    /// Command name.
    pub name: String,
    /// Human-readable description.
    pub description: Option<String>,
    /// Owning extension path.
    pub source: Option<String>,
}

/// A registered keyboard shortcut.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ShortcutRegistration {
    /// Key id (e.g. `ctrl+s`).
    pub key: String,
    /// Human-readable description.
    pub description: Option<String>,
    /// Owning extension path.
    pub extension_path: Option<String>,
}

/// A registered CLI flag.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FlagRegistration {
    /// Flag name.
    pub name: String,
    /// Human-readable description.
    pub description: Option<String>,
    /// Flag value type.
    pub kind: FlagKind,
    /// Default value (string form).
    pub default: Option<String>,
    /// Owning extension path.
    pub extension_path: Option<String>,
}

/// CLI flag value type.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FlagKind {
    /// Boolean flag.
    Boolean,
    /// String flag.
    String,
}

/// A registered message/tool/widget renderer.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RendererRegistration {
    /// Renderer placement kind.
    pub kind: RendererKind,
    /// Renderer name / key.
    pub name: String,
}

/// Where a renderer is plugged in.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RendererKind {
    /// Message renderer.
    Message,
    /// Tool renderer.
    Tool,
    /// Widget renderer.
    Widget,
}

/// A registered custom provider.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ProviderRegistration {
    /// Provider id.
    pub name: String,
    /// Optional base URL.
    pub base_url: Option<String>,
    /// API shape.
    pub api: Option<String>,
}

/// Aggregate of host registrations. First registration of a duplicated name/key
/// wins; later insertions are rejected.
#[derive(Default, Debug)]
pub struct Registry {
    tools: Vec<ToolRegistration>,
    commands: Vec<CommandRegistration>,
    shortcuts: Vec<ShortcutRegistration>,
    flags: Vec<FlagRegistration>,
    renderers: Vec<RendererRegistration>,
    providers: Vec<ProviderRegistration>,
}

impl Registry {
    /// Create an empty registry.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Register a tool. Returns `false` if a tool with this name exists.
    pub fn register_tool(&mut self, tool: ToolRegistration) -> bool {
        if self.tools.iter().any(|t| t.name == tool.name) {
            return false;
        }
        self.tools.push(tool);
        true
    }

    /// Register a command. Returns `false` on duplicate name.
    pub fn register_command(&mut self, command: CommandRegistration) -> bool {
        if self.commands.iter().any(|c| c.name == command.name) {
            return false;
        }
        self.commands.push(command);
        true
    }

    /// Register a shortcut. Returns `false` on duplicate key.
    pub fn register_shortcut(&mut self, shortcut: ShortcutRegistration) -> bool {
        if self.shortcuts.iter().any(|s| s.key == shortcut.key) {
            return false;
        }
        self.shortcuts.push(shortcut);
        true
    }

    /// Register a flag. Returns `false` on duplicate name.
    pub fn register_flag(&mut self, flag: FlagRegistration) -> bool {
        if self.flags.iter().any(|f| f.name == flag.name) {
            return false;
        }
        self.flags.push(flag);
        true
    }

    /// Register a renderer. Returns `false` on duplicate (kind, name).
    pub fn register_renderer(&mut self, renderer: RendererRegistration) -> bool {
        if self
            .renderers
            .iter()
            .any(|r| r.kind == renderer.kind && r.name == renderer.name)
        {
            return false;
        }
        self.renderers.push(renderer);
        true
    }

    /// Register a provider. Returns `false` on duplicate name.
    pub fn register_provider(&mut self, provider: ProviderRegistration) -> bool {
        if self.providers.iter().any(|p| p.name == provider.name) {
            return false;
        }
        self.providers.push(provider);
        true
    }

    /// Look up a tool by name.
    #[must_use]
    pub fn tool(&self, name: &str) -> Option<&ToolRegistration> {
        self.tools.iter().find(|t| t.name == name)
    }

    /// All registered tools.
    #[must_use]
    pub fn tools(&self) -> &[ToolRegistration] {
        &self.tools
    }

    /// All registered commands.
    #[must_use]
    pub fn commands(&self) -> &[CommandRegistration] {
        &self.commands
    }

    /// All registered shortcuts.
    #[must_use]
    pub fn shortcuts(&self) -> &[ShortcutRegistration] {
        &self.shortcuts
    }

    /// All registered flags.
    #[must_use]
    pub fn flags(&self) -> &[FlagRegistration] {
        &self.flags
    }

    /// All registered renderers.
    #[must_use]
    pub fn renderers(&self) -> &[RendererRegistration] {
        &self.renderers
    }

    /// All registered providers.
    #[must_use]
    pub fn providers(&self) -> &[ProviderRegistration] {
        &self.providers
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::protocol::{SlotPlacement, Style, StyledRun};
    use crate::test_support::make_pair;
    use futures::StreamExt;
    use pi_ai::types::ModelCost;
    use std::error::Error;

    type R = Result<(), Box<dyn Error>>;

    fn reg_tool(name: &str) -> ToolRegistration {
        ToolRegistration {
            name: name.to_owned(),
            label: name.to_owned(),
            description: "d".to_owned(),
            parameters: serde_json::json!({}),
            execution_mode: None,
        }
    }

    #[tokio::test]
    async fn extension_tool_preflight_prepares_then_validates_on_host() -> R {
        let (client, mut host) = make_pair().await;
        let tool = Arc::new(ExtensionAgentTool::new(
            reg_tool("ext.echo"),
            Arc::new(client),
        ));
        let driver = {
            let tool = Arc::clone(&tool);
            tokio::spawn(async move {
                tool.prepare_and_validate_arguments(
                    serde_json::json!({"raw": "7"})
                        .as_object()
                        .cloned()
                        .unwrap_or_default(),
                )
                .await
            })
        };

        let prepare = host.require_frame(methods::TOOL_PREPARE).await?;
        assert_eq!(prepare.payload["name"], "ext.echo");
        assert_eq!(prepare.payload["args"]["raw"], "7");
        host.write_frame(&Frame::response(
            prepare.id,
            Method::Notify,
            serde_json::json!({"args":{"value":7}}),
        ))
        .await?;

        let validate = host.require_frame(methods::TOOL_VALIDATE).await?;
        assert_eq!(validate.payload["name"], "ext.echo");
        assert_eq!(validate.payload["args"]["value"], 7);
        host.write_frame(&Frame::response(
            validate.id,
            Method::Notify,
            serde_json::json!({"args":{"value":7,"valid":true}}),
        ))
        .await?;

        let result = driver.await??;
        assert_eq!(result["value"], 7);
        assert_eq!(result["valid"], true);
        Ok(())
    }

    #[tokio::test]
    async fn extension_tool_proxies_execute_and_progress() -> R {
        let (client, mut host) = make_pair().await;
        let tool = ExtensionAgentTool::new(reg_tool("ext.echo"), Arc::new(client));
        let cancel = CancellationToken::new();
        let seen = Arc::new(std::sync::Mutex::new(Vec::<String>::new()));
        let updates = ToolUpdates::new({
            let seen = Arc::clone(&seen);
            move |partial: AgentToolResult| {
                let text = partial
                    .content
                    .iter()
                    .find_map(|block| match block {
                        pi_ai::ToolResultContent::Text(text) => Some(text.text.clone()),
                        pi_ai::ToolResultContent::Image(_) => None,
                    })
                    .unwrap_or_default();
                seen.lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .push(text);
            }
        });
        let exec_fut = tool.execute(
            "call-1",
            serde_json::json!({"prompt": "hi"})
                .as_object()
                .cloned()
                .ok_or("args object")?,
            cancel,
            updates,
        );
        let driver = tokio::spawn(exec_fut);
        // Host reads execute req, streams a toolUpdate, then terminal result.
        let req = host.require_frame("tool.execute").await?;
        assert_eq!(req.method, methods::TOOL_EXECUTE);
        assert_eq!(req.payload["name"], "ext.echo");
        assert_eq!(req.payload["toolCallId"], "call-1");
        assert_eq!(req.payload["args"]["prompt"], "hi");
        let partial = serde_json::json!({
            "content": [{"type":"text","text":"half"}],
            "details": {},
        });
        let ev = Frame::event(
            req.id,
            Method::ToolUpdate,
            serde_json::json!({
                "toolCallId": "call-1",
                "toolName": "ext.echo",
                "partialResult": partial,
            }),
        );
        host.write_frame(&ev).await?;
        // Allow the adapter task to deliver progress before the terminal frame.
        tokio::task::yield_now().await;
        let result_payload = serde_json::json!({
            "content": [{"type":"text","text":"done"}],
            "details": {"ok": true},
        });
        let res = Frame::response(req.id, Method::Notify, result_payload);
        host.write_frame(&res).await?;
        let result = driver.await??;
        assert_eq!(
            result.content.iter().find_map(|block| match block {
                pi_ai::ToolResultContent::Text(text) => Some(text.text.as_str()),
                pi_ai::ToolResultContent::Image(_) => None,
            }),
            Some("done")
        );
        assert_eq!(result.details["ok"], true);
        let progress = seen
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone();
        assert_eq!(progress, vec!["half".to_owned()]);
        Ok(())
    }

    #[tokio::test]
    async fn extension_tool_cancel_sends_tool_cancel_and_errors() -> R {
        let (client, mut host) = make_pair().await;
        let tool = ExtensionAgentTool::new(reg_tool("ext.slow"), Arc::new(client));
        let cancel = CancellationToken::new();
        let exec_fut = tool.execute(
            "call-cancel",
            Map::new(),
            cancel.clone(),
            ToolUpdates::noop(),
        );
        let driver = tokio::spawn(exec_fut);
        let req = host.require_frame("tool.execute").await?;
        assert_eq!(req.method, methods::TOOL_EXECUTE);
        cancel.cancel();
        let cancel_frame = host.require_frame("tool.cancel").await?;
        assert_eq!(cancel_frame.method, methods::TOOL_CANCEL);
        assert_eq!(cancel_frame.payload["id"], req.id);
        let Err(err) = driver.await? else {
            return Err("cancelled tool must return ToolError".into());
        };
        assert!(
            err.to_string().contains("cancelled"),
            "unexpected cancel error: {err}"
        );
        Ok(())
    }

    // Bring Method into scope for the adapter tests.
    use crate::protocol::Method;

    fn sample_assistant_partial() -> Value {
        serde_json::json!({
            "role": "assistant",
            "content": [],
            "api": "custom",
            "provider": "custom",
            "model": "m",
            "usage": {
                "input": 0,
                "output": 0,
                "cacheRead": 0,
                "cacheWrite": 0,
                "totalTokens": 0,
                "cost": {
                    "input": 0.0,
                    "output": 0.0,
                    "cacheRead": 0.0,
                    "cacheWrite": 0.0,
                    "total": 0.0
                }
            },
            "stopReason": "stop",
            "timestamp": 0
        })
    }

    fn text_delta_event(delta: &str) -> Value {
        serde_json::json!({
            "type": "text_delta",
            "contentIndex": 0,
            "delta": delta,
            "partial": sample_assistant_partial(),
        })
    }

    #[tokio::test]
    async fn extension_provider_forwards_prepared_stream_options() -> R {
        use pi_ai::types::{CacheRetention, Transport};
        use std::collections::BTreeMap;

        let (client, mut host) = make_pair().await;
        let provider = ExtensionProvider::new("custom", Arc::new(client));
        let model = Model {
            id: "m".to_owned(),
            name: "M".to_owned(),
            api: "custom-api".to_owned(),
            provider: "custom".to_owned(),
            base_url: "https://custom.example/v1".to_owned(),
            ..base_model_defaults()
        };
        let mut headers = BTreeMap::new();
        headers.insert(
            "Authorization".to_owned(),
            Some("Bearer sk-runtime".to_owned()),
        );
        headers.insert("X-Drop".to_owned(), None);
        let mut env = BTreeMap::new();
        env.insert("CUSTOM_REGION".to_owned(), "us-test".to_owned());
        let mut extra = Map::new();
        extra.insert("reasoning".to_owned(), Value::String("high".to_owned()));
        extra.insert(
            "thinkingBudgets".to_owned(),
            serde_json::json!({"high": 4096}),
        );
        let options = StreamOptions {
            temperature: Some(0.2),
            max_tokens: Some(128),
            api_key: Some("sk-runtime".to_owned()),
            transport: Some(Transport::Sse),
            cache_retention: Some(CacheRetention::Short),
            session_id: Some("sess-1".to_owned()),
            headers: Some(headers),
            timeout_ms: Some(12_000),
            websocket_connect_timeout_ms: Some(1_500),
            max_retries: Some(2),
            max_retry_delay_ms: Some(5_000),
            metadata: Some(
                serde_json::json!({"user_id": "u-1"})
                    .as_object()
                    .cloned()
                    .ok_or("metadata object")?,
            ),
            env: Some(env),
            extra,
            ..StreamOptions::default()
        };
        let mut stream = provider.stream(&model, Context::default(), options);
        let req = host.require_frame("provider.stream").await?;
        assert_eq!(req.method, methods::PROVIDER_STREAM);
        assert_eq!(req.payload["providerId"], "custom");
        assert_eq!(req.payload["model"]["id"], "m");
        assert_eq!(req.payload["model"]["api"], "custom-api");
        assert_eq!(req.payload["model"]["baseUrl"], "https://custom.example/v1");
        let opts = &req.payload["options"];
        assert_eq!(opts["temperature"], 0.2);
        assert_eq!(opts["maxTokens"], 128);
        assert_eq!(opts["apiKey"], "sk-runtime");
        assert_eq!(opts["transport"], "sse");
        assert_eq!(opts["cacheRetention"], "short");
        assert_eq!(opts["sessionId"], "sess-1");
        assert_eq!(opts["headers"]["Authorization"], "Bearer sk-runtime");
        assert!(opts["headers"]["X-Drop"].is_null());
        assert_eq!(opts["timeoutMs"], 12_000);
        assert_eq!(opts["websocketConnectTimeoutMs"], 1_500);
        assert_eq!(opts["maxRetries"], 2);
        assert_eq!(opts["maxRetryDelayMs"], 5_000);
        assert_eq!(opts["metadata"]["user_id"], "u-1");
        assert_eq!(opts["env"]["CUSTOM_REGION"], "us-test");
        assert_eq!(opts["reasoning"], "high");
        assert_eq!(opts["thinkingBudgets"]["high"], 4096);
        // Credentials must stay inside options — not at the request root.
        assert!(req.payload.get("apiKey").is_none());
        assert!(req.payload.get("headers").is_none());
        assert!(req.payload.get("env").is_none());

        let terminal = Frame::response(req.id, Method::Notify, serde_json::json!({}));
        host.write_frame(&terminal).await?;
        let ended = tokio::time::timeout(Duration::from_secs(2), stream.next()).await;
        assert!(
            matches!(ended, Ok(None)),
            "provider stream should terminate cleanly, got {ended:?}"
        );
        Ok(())
    }

    #[tokio::test]
    async fn extension_provider_streams_ordered_events() -> R {
        let (client, mut host) = make_pair().await;
        let provider = ExtensionProvider::new("custom", Arc::new(client));
        let model = Model {
            id: "m".to_owned(),
            name: "M".to_owned(),
            api: "custom".to_owned(),
            provider: "custom".to_owned(),
            ..base_model_defaults()
        };
        let mut stream = provider.stream(&model, Context::default(), StreamOptions::default());
        let req = host.require_frame("provider.stream").await?;

        // Bare assistant event payload.
        host.write_frame(&Frame::event(
            req.id,
            Method::ProviderEvent,
            text_delta_event("one"),
        ))
        .await?;
        // Wrapped ProviderEvent payload (host may emit either shape).
        host.write_frame(&Frame::event(
            req.id,
            Method::ProviderEvent,
            serde_json::json!({
                "providerId": "custom",
                "callId": format!("{}", req.id),
                "event": "text_delta",
                "data": text_delta_event("two"),
            }),
        ))
        .await?;
        host.write_frame(&Frame::response(
            req.id,
            Method::Notify,
            serde_json::json!({}),
        ))
        .await?;

        let first = tokio::time::timeout(Duration::from_secs(2), stream.next())
            .await?
            .ok_or("missing first event")??;
        let second = tokio::time::timeout(Duration::from_secs(2), stream.next())
            .await?
            .ok_or("missing second event")??;
        match (first, second) {
            (
                AssistantMessageEvent::TextDelta { delta: a, .. },
                AssistantMessageEvent::TextDelta { delta: b, .. },
            ) => {
                assert_eq!(a, "one");
                assert_eq!(b, "two");
            }
            other => return Err(format!("unexpected event order: {other:?}").into()),
        }
        let ended = tokio::time::timeout(Duration::from_secs(2), stream.next()).await?;
        assert!(ended.is_none(), "expected clean EOS, got {ended:?}");
        Ok(())
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn real_host_source_provider_streams_start_text_done_and_tears_down() -> R {
        use crate::host::{HostSource, HostSpec};
        use std::path::Path;

        let workspace = Path::new(env!("CARGO_MANIFEST_DIR"))
            .parent()
            .and_then(Path::parent)
            .ok_or("workspace root")?;
        let host_path = workspace
            .join("packages")
            .join("extension-host")
            .join("dist")
            .join("pi-extension-host");
        assert!(
            host_path.is_file(),
            "compiled host missing: {}",
            host_path.display()
        );
        let extension_path = workspace
            .join("scripts")
            .join("verification")
            .join("extension.ts");
        let spec = HostSpec {
            program: host_path.clone(),
            args: vec!["--cwd".into(), workspace.to_string_lossy().into_owned()],
            source: HostSource::Env(host_path),
        };
        let client = Arc::new(HostClient::spawn(&spec)?);
        client.handshake().await?;
        let loaded = client
            .request_raw(
                "extensions.load",
                serde_json::json!({
                    "extensionPaths": [extension_path.to_string_lossy()],
                    "cwd": workspace.to_string_lossy(),
                }),
                Duration::from_secs(5),
            )
            .await?;
        // Built-in extensions (llama.cpp) register before user extensions, so
        // find the fixture provider by name rather than position.
        let providers = loaded.payload["providers"]
            .as_array()
            .cloned()
            .unwrap_or_default();
        let verification = providers
            .iter()
            .find(|provider| provider["name"] == "verification")
            .cloned()
            .unwrap_or_default();
        assert_eq!(verification["name"], "verification");
        assert_eq!(verification["streamSimple"], true);

        let provider = ExtensionProvider::new("verification", Arc::clone(&client));
        let model = Model {
            id: "model".to_owned(),
            name: "Verification Model".to_owned(),
            api: "verification".to_owned(),
            provider: "verification".to_owned(),
            base_url: "https://verification.invalid".to_owned(),
            ..base_model_defaults()
        };
        let mut stream = provider.stream(&model, Context::default(), StreamOptions::default());
        let events = tokio::time::timeout(Duration::from_secs(5), async {
            let mut events = Vec::new();
            while let Some(event) = stream.next().await {
                events.push(event?);
            }
            Ok::<_, ProviderError>(events)
        })
        .await??;

        assert!(matches!(
            events.first(),
            Some(AssistantMessageEvent::Start { .. })
        ));
        let marker_index = events
            .iter()
            .position(|event| matches!(event, AssistantMessageEvent::TextDelta { delta, .. } if delta.contains("PI_VERIFICATION_FINAL")))
            .ok_or("missing verification marker text event")?;
        let done_count = events
            .iter()
            .filter(|event| matches!(event, AssistantMessageEvent::Done { .. }))
            .count();
        assert_eq!(done_count, 1, "expected exactly one done event: {events:?}");
        assert!(
            matches!(events.last(), Some(AssistantMessageEvent::Done { .. })),
            "done must be the final event: {events:?}"
        );
        assert!(
            marker_index < events.len() - 1,
            "text must precede done: {events:?}"
        );

        tokio::time::timeout(Duration::from_secs(2), client.shutdown()).await??;
        assert!(!client.is_running());
        Ok(())
    }

    #[tokio::test]
    async fn extension_provider_cancel_sends_provider_cancel() -> R {
        let (client, mut host) = make_pair().await;
        let provider = ExtensionProvider::new("custom", Arc::new(client));
        let model = Model {
            id: "m".to_owned(),
            name: "M".to_owned(),
            api: "custom".to_owned(),
            provider: "custom".to_owned(),
            ..base_model_defaults()
        };
        let cancel = CancellationToken::new();
        let options = StreamOptions {
            signal: Some(cancel.clone()),
            ..StreamOptions::default()
        };
        let mut stream = provider.stream(&model, Context::default(), options);
        let req = host.require_frame("provider.stream").await?;
        cancel.cancel();
        let cancel_frame = host.require_frame("provider.cancel").await?;
        assert_eq!(cancel_frame.method, methods::PROVIDER_CANCEL);
        assert_eq!(cancel_frame.payload["id"], req.id);
        let item = tokio::time::timeout(Duration::from_secs(2), stream.next())
            .await?
            .ok_or("missing cancel error")?;
        let Err(err) = item else {
            return Err("cancelled stream must yield Err".into());
        };
        assert!(
            err.to_string().contains("cancelled"),
            "unexpected cancel error: {err}"
        );
        Ok(())
    }

    #[tokio::test]
    async fn extension_provider_host_error_surfaces() -> R {
        use crate::protocol::ErrorPayload;

        let (client, mut host) = make_pair().await;
        let provider = ExtensionProvider::new("custom", Arc::new(client));
        let model = Model {
            id: "m".to_owned(),
            name: "M".to_owned(),
            api: "custom".to_owned(),
            provider: "custom".to_owned(),
            ..base_model_defaults()
        };
        let mut stream = provider.stream(&model, Context::default(), StreamOptions::default());
        let req = host.require_frame("provider.stream").await?;
        let error = ErrorPayload::new("extension_error", "host provider failed");
        host.write_frame(&Frame::error_frame(req.id, Method::Notify, &error)?)
            .await?;
        let item = tokio::time::timeout(Duration::from_secs(2), stream.next())
            .await?
            .ok_or("missing host error")?;
        let Err(err) = item else {
            return Err("host error must yield Err".into());
        };
        assert!(
            err.to_string().contains("host provider failed")
                || err.to_string().contains("extension_error"),
            "unexpected host error: {err}"
        );
        Ok(())
    }

    #[tokio::test]
    async fn extension_provider_streams_events_and_ends() -> R {
        let (client, mut host) = make_pair().await;
        let provider = ExtensionProvider::new("custom", Arc::new(client));
        let model = Model {
            id: "m".to_owned(),
            name: "M".to_owned(),
            api: "custom".to_owned(),
            provider: "custom".to_owned(),
            ..base_model_defaults()
        };
        let context = Context::default();
        let options = StreamOptions::default();
        let mut stream = provider.stream(&model, context, options);
        let req = host.require_frame("provider.stream").await?;
        assert_eq!(req.method, methods::PROVIDER_STREAM);
        // Host sends the terminal response; the driver must close the stream
        // (drop its sender) so the consumer observes end-of-stream, never hang.
        let terminal = Frame::response(req.id, Method::Notify, serde_json::json!({}));
        host.write_frame(&terminal).await?;
        let ended = tokio::time::timeout(Duration::from_secs(2), stream.next()).await;
        assert!(
            matches!(ended, Ok(None)),
            "provider stream should terminate cleanly, got {ended:?}"
        );
        Ok(())
    }
    #[test]
    fn linked_styled_run_emits_balanced_safe_osc8_and_keeps_buffer_style() {
        use std::cell::RefCell;

        let slot = crate::protocol::UiSlot {
            key: "links".to_owned(),
            generation: 1,
            placement: SlotPlacement::AboveEditor,
            height: 1,
            runs: vec![vec![
                StyledRun {
                    text: "safe".to_owned(),
                    style: Style {
                        bold: Some(true),
                        fg: Some(WireColor::Named {
                            name: NamedColor::Red,
                        }),
                        link: Some(crate::protocol::Hyperlink {
                            id: Some("docs".to_owned()),
                            uri: "https://safe.example/docs".to_owned(),
                        }),
                        ..Style::default()
                    },
                },
                StyledRun {
                    text: "bad\u{1b}]8;;https://evil.example".to_owned(),
                    style: Style {
                        link: Some(crate::protocol::Hyperlink {
                            id: None,
                            uri: "javascript:alert(1)\u{1b}".to_owned(),
                        }),
                        ..Style::default()
                    },
                },
            ]],
            focusable: false,
            cursor: None,
            overlay_options: None,
        };
        let mut component = SlotComponent::from_ui_slot(&slot);
        let mut buffer = Buffer::empty(Rect::new(0, 0, 3, 1));
        let annotations = RefCell::new(pi_tui::frame::FrameAnnotations::new());
        pi_tui::frame::with_annotations(&annotations, || {
            component.render(buffer.area, &mut buffer);
        });

        assert!(
            buffer
                .cell((0, 0))
                .is_some_and(|cell| cell.modifier.contains(Modifier::BOLD))
        );
        let annotations = annotations.into_inner();
        assert_eq!(annotations.raw_regions().len(), 1);
        assert_eq!(annotations.raw_regions()[0].area.width, 3);
        let raw = String::from_utf8_lossy(&annotations.raw_regions()[0].bytes);
        let open = "\u{1b}]8;id=docs;https://safe.example/docs\u{1b}\\";
        let close = "\u{1b}]8;;\u{1b}\\";
        assert_eq!(raw.matches(open).count(), 1);
        assert_eq!(raw.matches(close).count(), 1);
        assert!(raw.contains("\u{1b}[1;31msaf\u{1b}[0m"));
        assert!(!raw.contains("javascript:"));
        assert!(!raw.contains("evil.example"));
    }

    fn base_model_defaults() -> Model {
        Model {
            id: String::new(),
            name: String::new(),
            api: String::new(),
            provider: String::new(),
            base_url: String::new(),
            reasoning: false,
            thinking_level_map: None,
            input: Vec::new(),
            cost: ModelCost::default(),
            context_window: 0,
            max_tokens: 0,
            headers: None,
            compat: None,
            extra: std::collections::BTreeMap::new(),
        }
    }

    #[tokio::test]
    async fn slot_component_renders_sanitized_runs_no_escape_leak() -> R {
        let mut hostile = crate::protocol::UiSlot {
            key: "w".to_owned(),
            generation: 1,
            placement: SlotPlacement::AboveEditor,
            height: 1,
            runs: vec![vec![StyledRun {
                text: "R\u{1b}[31med\u{1b}[0m".to_owned(),
                style: Style {
                    bold: Some(true),
                    ..Style::default()
                },
            }]],
            focusable: false,
            cursor: None,
            overlay_options: None,
        };
        let mut component = SlotComponent::from_ui_slot(&hostile);
        assert_eq!(component.measure(20), 1);
        let mut buf = Buffer::empty(Rect::new(0, 0, 20, 1));
        component.render(buf.area, &mut buf);
        let text: String = (0..20)
            .map(|x| {
                buf.cell((x, 0))
                    .map_or(' ', |c| c.symbol().chars().next().unwrap_or(' '))
            })
            .collect();
        assert!(
            !text.contains('\u{1b}'),
            "escape leaked into buffer: {text:?}"
        );
        assert!(text.starts_with("Red"), "unexpected render: {text:?}");
        assert!(
            buf.cell((0, 0))
                .is_some_and(|cell| cell.modifier.contains(Modifier::BOLD)),
            "structured bold style must survive projection"
        );
        let _ = &mut hostile;
        Ok(())
    }

    #[test]
    fn focused_slot_component_publishes_clamped_hardware_cursor() {
        use std::cell::RefCell;

        let slot = crate::protocol::UiSlot {
            key: "cursor".to_owned(),
            generation: 1,
            placement: SlotPlacement::AboveEditor,
            height: 2,
            runs: vec![vec![StyledRun {
                text: "x".to_owned(),
                style: Style::default(),
            }]],
            focusable: true,
            cursor: Some(crate::protocol::SlotCursor { col: 99, row: 99 }),
            overlay_options: None,
        };
        let mut component = SlotComponent::from_ui_slot(&slot);
        component.set_focused(true);
        let area = Rect::new(4, 6, 3, 2);
        let mut buffer = Buffer::empty(Rect::new(0, 0, 10, 10));
        let annotations = RefCell::new(pi_tui::frame::FrameAnnotations::new());
        pi_tui::frame::with_annotations(&annotations, || component.render(area, &mut buffer));
        assert_eq!(
            annotations.into_inner().cursor(),
            Some(Position { x: 6, y: 7 })
        );
    }

    #[tokio::test]
    async fn slot_component_forwards_input_when_focused() -> R {
        use crossterm::event::{KeyCode, KeyEvent, KeyEventKind};
        let slot = crate::protocol::UiSlot {
            key: "w".to_owned(),
            generation: 1,
            placement: SlotPlacement::AboveEditor,
            height: 1,
            runs: vec![vec![StyledRun {
                text: "x".to_owned(),
                style: Style::default(),
            }]],
            focusable: true,
            cursor: None,
            overlay_options: None,
        };
        let (tx, mut rx) = mpsc::unbounded_channel::<UiEventWire>();
        let mut component = SlotComponent::from_ui_slot(&slot).with_event_router(tx);
        assert_eq!(
            component.handle_event(&UiEvent::Key(KeyEvent::new(
                KeyCode::Enter,
                KeyModifiers::empty(),
            ))),
            EventResult::Ignored,
            "unfocused component must ignore input"
        );
        component.set_focused(true);
        assert_eq!(
            component.handle_event(&UiEvent::Key(KeyEvent::new_with_kind(
                KeyCode::Char('a'),
                KeyModifiers::CONTROL,
                KeyEventKind::Press,
            ))),
            EventResult::Consumed
        );
        let wire = rx.recv().await.ok_or("no forwarded event")?;
        match wire {
            UiEventWire::Key {
                code, modifiers, ..
            } => {
                assert_eq!(code, "a");
                assert_eq!(modifiers.ctrl, Some(true));
            }
            other => return Err(format!("expected Key wire, got {other:?}").into()),
        }
        Ok(())
    }

    #[tokio::test]
    async fn extension_tool_registration_and_timeout_builders() -> R {
        let (client, _host) = make_pair().await;
        let meta = ToolRegistration {
            name: "ext.meta".to_owned(),
            label: "Meta".to_owned(),
            description: "desc".to_owned(),
            parameters: serde_json::json!({"type": "object"}),
            execution_mode: Some(ToolExecutionMode::Sequential),
        };
        let tool = ExtensionAgentTool::new(meta, Arc::new(client))
            .with_timeout(Duration::from_secs(9))
            .with_timeout(Duration::from_millis(50));
        let registration = tool.registration();
        assert_eq!(registration.name, "ext.meta");
        assert_eq!(registration.label, "Meta");
        assert_eq!(registration.description, "desc");
        assert_eq!(
            registration.parameters,
            serde_json::json!({"type": "object"})
        );
        assert_eq!(
            registration.execution_mode,
            Some(ToolExecutionMode::Sequential)
        );

        // Last with_timeout replaces earlier values; hung host hits the 50ms deadline.
        let err = match tool.prepare_and_validate_arguments(Map::new()).await {
            Err(err) => err,
            Ok(value) => {
                return Err(format!("hung host must time out, got {value:?}").into());
            }
        };
        assert!(
            err.to_string().contains("timed out after 50ms"),
            "unexpected: {err}"
        );
        Ok(())
    }

    #[tokio::test]
    async fn extension_provider_timeout_builders() -> R {
        let (client, _host) = make_pair().await;
        let provider = ExtensionProvider::new("custom", Arc::new(client))
            .with_timeout(Duration::from_secs(9))
            .with_timeout(Duration::from_millis(50));
        let model = Model {
            id: "m".to_owned(),
            name: "M".to_owned(),
            api: "custom".to_owned(),
            provider: "custom".to_owned(),
            ..base_model_defaults()
        };
        let mut stream = provider.stream(&model, Context::default(), StreamOptions::default());
        // Last with_timeout replaces earlier values; hung host hits the 50ms deadline.
        let item = tokio::time::timeout(Duration::from_secs(2), stream.next())
            .await
            .map_err(|_| "provider timeout exceeded outer guard")?
            .ok_or("hung host must yield a stream item")?;
        let err = match item {
            Err(err) => err,
            Ok(value) => {
                return Err(format!("hung host must time out, got {value:?}").into());
            }
        };
        assert!(
            err.to_string().contains("timed out after 50ms"),
            "unexpected: {err}"
        );
        Ok(())
    }

    #[tokio::test]
    async fn extension_provider_idle_deadline_errors_without_finish_wait() -> R {
        let (client, mut host) = make_pair().await;
        let provider = ExtensionProvider::new("custom", Arc::new(client))
            .with_timeout(Duration::from_millis(50));
        let model = Model {
            id: "m".to_owned(),
            name: "M".to_owned(),
            api: "custom".to_owned(),
            provider: "custom".to_owned(),
            ..base_model_defaults()
        };
        let mut stream = provider.stream(&model, Context::default(), StreamOptions::default());
        let req = host.require_frame(methods::PROVIDER_STREAM).await?;
        let req_id = req.id;

        // Host: no events; late terminal at 80ms sits inside a finish(50ms)
        // window that would begin at the 50ms idle expiry. Then observe cancel.
        let host_task = tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(80)).await;
            host.write_frame(&Frame::response(
                req_id,
                Method::Notify,
                serde_json::json!({}),
            ))
            .await
            .map_err(|e| e.to_string())?;
            let cancel = tokio::time::timeout(
                Duration::from_secs(2),
                host.require_frame(methods::PROVIDER_CANCEL),
            )
            .await
            .map_err(|_| "PROVIDER_CANCEL not received".to_owned())?
            .map_err(|e| e.to_string())?;
            Ok::<_, String>(cancel)
        });

        let item = tokio::time::timeout(Duration::from_secs(2), stream.next())
            .await
            .map_err(|_| "idle deadline exceeded outer guard")?
            .ok_or("idle deadline must yield Timeout, not clean EOF")?;
        let err = match item {
            Err(err) => err,
            Ok(value) => {
                return Err(format!("idle deadline must be Timeout, got Ok({value:?})").into());
            }
        };
        assert!(
            err.to_string().contains("timed out after 50ms"),
            "unexpected: {err}"
        );

        let cancel = host_task.await??;
        assert_eq!(cancel.method, methods::PROVIDER_CANCEL);
        assert_eq!(cancel.payload["id"], req_id);
        Ok(())
    }

    #[tokio::test]
    async fn extension_provider_post_first_event_gaps_are_unbounded() -> R {
        let (client, mut host) = make_pair().await;
        let provider = ExtensionProvider::new("custom", Arc::new(client))
            .with_timeout(Duration::from_millis(50));
        let model = Model {
            id: "m".to_owned(),
            name: "M".to_owned(),
            api: "custom".to_owned(),
            provider: "custom".to_owned(),
            ..base_model_defaults()
        };
        let mut stream = provider.stream(&model, Context::default(), StreamOptions::default());
        let req = host.require_frame(methods::PROVIDER_STREAM).await?;

        host.write_frame(&Frame::event(
            req.id,
            Method::ProviderEvent,
            text_delta_event("hi"),
        ))
        .await?;
        // Gap longer than `with_timeout` — must not abort a live stream.
        tokio::time::sleep(Duration::from_millis(100)).await;
        host.write_frame(&Frame::response(
            req.id,
            Method::Notify,
            serde_json::json!({}),
        ))
        .await?;

        let first = tokio::time::timeout(Duration::from_secs(2), stream.next())
            .await?
            .ok_or("missing first event")??;
        match first {
            AssistantMessageEvent::TextDelta { delta, .. } => assert_eq!(delta, "hi"),
            other => return Err(format!("unexpected first event: {other:?}").into()),
        }
        let ended = tokio::time::timeout(Duration::from_secs(2), stream.next()).await?;
        assert!(
            ended.is_none(),
            "expected clean EOS after post-first-event gap, got {ended:?}"
        );
        Ok(())
    }

    #[tokio::test]
    async fn registry_first_registration_wins() {
        let mut registry = Registry::new();
        assert!(registry.register_tool(reg_tool("t")));
        assert!(
            !registry.register_tool(reg_tool("t")),
            "duplicate must lose"
        );
        assert_eq!(registry.tools().len(), 1);

        assert!(registry.register_command(CommandRegistration {
            name: "c".to_owned(),
            description: None,
            source: None,
        }));
        assert!(!registry.register_command(CommandRegistration {
            name: "c".to_owned(),
            description: None,
            source: None,
        }));

        assert!(registry.register_provider(ProviderRegistration {
            name: "p".to_owned(),
            base_url: None,
            api: None,
        }));
        assert!(!registry.register_provider(ProviderRegistration {
            name: "p".to_owned(),
            base_url: None,
            api: None,
        }));
        assert!(registry.tool("t").is_some());
    }
}
