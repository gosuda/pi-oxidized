//! Compact, replayable progress frames for one assistant response.
//!
//! Terminal settlement is intentionally excluded from [`AssistantMessageFrame`]
//! and remains the caller's responsibility. The encoder consumes provider
//! events while they are still backed by a shared live partial message; the
//! reducer reconstructs an owned message from persisted frames.

use std::borrow::Borrow;
use std::collections::HashMap;
use std::sync::Arc;

use serde::ser::{SerializeMap, SerializeSeq};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

use crate::providers::shared::parse_streaming_json;
use crate::types::{
    AssistantContent, AssistantMessage, AssistantMessageEvent, StopReason, TextContent,
    ThinkingContent, ToolCall,
};

/// The largest content index JavaScript can represent without losing an
/// integer bit when this wire record is consumed by the TypeScript runtime.
const MAX_SAFE_INTEGER: u64 = 9_007_199_254_740_991;

/// Compact, replayable assistant-message progress.
///
/// The `done` and `error` terminal events are deliberately not represented:
/// callers persist the frames first and settle the assistant message in their
/// own terminal record.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(tag = "type", rename_all = "camelCase")]
pub enum AssistantMessageFrame {
    /// Begins an assistant response with an empty content accumulator.
    ///
    /// The payload is a non-terminal header snapshot, never a settled stop or
    /// error result; later frames append the content blocks onto it.
    #[serde(rename = "start")]
    Start {
        /// Message header the reducer replays onto: api, provider, model,
        /// timestamp, response identifiers, diagnostics and usage as observed
        /// at the start event, with the stop reason still pending.
        partial: Box<AssistantMessage>,
    },
    /// Begins a text content block.
    #[serde(rename = "text_start")]
    TextStart {
        /// Zero-based position of the block in the assistant message's content
        /// list. Start frames must claim indices in ascending order so the
        /// reducer appends without gaps or collisions, and the value must stay
        /// within JavaScript's safe integer range for the TypeScript runtime.
        #[serde(rename = "contentIndex")]
        content_index: u64,
        /// Block as observed when it opened, including text the provider had
        /// already produced. Later delta frames carry only what this snapshot
        /// does not already cover.
        content: TextContent,
    },
    /// Appends text to a text content block.
    #[serde(rename = "text_delta")]
    TextDelta {
        /// Zero-based index of the block this frame continues, as claimed by
        /// that block's start frame.
        #[serde(rename = "contentIndex")]
        content_index: u64,
        /// Text to append. Offsets are counted in UTF-16 units to match the
        /// TypeScript runtime, and any leading portion already covered by an
        /// earlier snapshot is stripped before the frame is emitted.
        delta: String,
    },
    /// Completes a text content block.
    #[serde(rename = "text_end")]
    TextEnd {
        /// Zero-based index of the block being closed, as claimed by its start
        /// frame.
        #[serde(rename = "contentIndex")]
        content_index: u64,
        /// Final text for the block. The reducer replaces whatever the start
        /// snapshot and deltas accumulated rather than appending to it.
        content: String,
        /// Provider-specific signature carried alongside the finished text,
        /// used when replaying the block back to the provider; absent when the
        /// provider supplies none.
        #[serde(rename = "textSignature", skip_serializing_if = "Option::is_none")]
        text_signature: Option<String>,
    },
    /// Begins a thinking content block.
    #[serde(rename = "thinking_start")]
    ThinkingStart {
        /// Zero-based index at which the reducer appends the reasoning block,
        /// matching the position in the provider's partial message.
        #[serde(rename = "contentIndex")]
        content_index: u64,
        /// Reasoning block as observed when it opened, including thinking text
        /// already produced and any signature or redaction marker attached to
        /// it. Later delta frames carry only the uncovered remainder.
        content: ThinkingContent,
    },
    /// Appends text to a thinking content block.
    #[serde(rename = "thinking_delta")]
    ThinkingDelta {
        /// Zero-based index of the reasoning block this frame continues, as
        /// claimed by that block's start frame.
        #[serde(rename = "contentIndex")]
        content_index: u64,
        /// Reasoning text to append, with any leading portion already covered
        /// by an earlier snapshot stripped and offsets counted in UTF-16 units.
        delta: String,
    },
    /// Completes a thinking content block.
    #[serde(rename = "thinking_end")]
    ThinkingEnd {
        /// Zero-based index of the reasoning block being closed, as claimed by
        /// its start frame.
        #[serde(rename = "contentIndex")]
        content_index: u64,
        /// Final reasoning text, which the reducer copies over the accumulated
        /// snapshot plus deltas.
        content: String,
        /// Provider-specific reasoning signature or encrypted reasoning
        /// payload for the finished block; absent when the provider supplies
        /// none.
        #[serde(rename = "thinkingSignature", skip_serializing_if = "Option::is_none")]
        thinking_signature: Option<String>,
        /// Whether safety filters redacted the reasoning text. The reducer
        /// assigns this onto the finished block, so `None` records a provider
        /// that never reported redaction.
        #[serde(skip_serializing_if = "Option::is_none")]
        redacted: Option<bool>,
    },
    /// Begins a tool-call content block.
    #[serde(rename = "toolcall_start")]
    ToolCallStart {
        /// Zero-based index at which the reducer appends the tool-call block,
        /// matching the position in the provider's partial message.
        #[serde(rename = "contentIndex")]
        content_index: u64,
        /// Invocation as observed when the block opened: identifier, tool name,
        /// and the arguments parsed from the stream so far. When those
        /// arguments are already non-empty the encoder is behind the live
        /// snapshot and resynchronizes with a checkpoint frame.
        #[serde(rename = "toolCall")]
        tool_call: ToolCall,
    },
    /// Replaces the parsed tool-call argument checkpoint.
    #[serde(rename = "toolcall_checkpoint")]
    ToolCallCheckpoint {
        /// Zero-based index of the tool-call block whose argument text is
        /// resynchronized.
        #[serde(rename = "contentIndex")]
        content_index: u64,
        /// Authoritative partial-JSON argument stream, serialized. The reducer
        /// discards its own accumulated text for this block and reparses this
        /// string, so replay never double-applies deltas the encoder had
        /// already folded into a snapshot.
        json: String,
    },
    /// Appends serialized tool-call argument data.
    #[serde(rename = "toolcall_delta")]
    ToolCallDelta {
        /// Zero-based index of the tool-call block this frame continues, as
        /// claimed by that block's start frame.
        #[serde(rename = "contentIndex")]
        content_index: u64,
        /// Argument JSON to append to the block's running text. The slice may
        /// split a token or a string literal mid-way; it only yields complete
        /// arguments at the end frame.
        delta: String,
    },
    /// Completes a tool-call content block.
    #[serde(rename = "toolcall_end")]
    ToolCallEnd {
        /// Zero-based index of the tool-call block being closed, as claimed by
        /// its start frame.
        #[serde(rename = "contentIndex")]
        content_index: u64,
        /// Final provider-assigned invocation identifier, replacing whatever
        /// the start snapshot carried.
        id: String,
        /// Final registered tool name the invocation resolved to.
        name: String,
        /// Complete argument object, replacing the partial arguments parsed
        /// from the checkpoint and delta frames.
        arguments: Map<String, Value>,
        /// Provider-specific opaque signature that lets a later request reuse
        /// the model's reasoning attached to this call; absent when the
        /// provider supplies none.
        #[serde(rename = "thoughtSignature", skip_serializing_if = "Option::is_none")]
        thought_signature: Option<String>,
        /// Namespace qualifying the tool name, carried for dynamically loaded
        /// or namespaced tool sets and absent for plain registered tools.
        #[serde(skip_serializing_if = "Option::is_none")]
        namespace: Option<String>,
    },
}

/// An encoder or reducer rejected an invalid frame transition or payload.
#[derive(Clone, Debug, Eq, PartialEq, thiserror::Error)]
#[error("{message}")]
pub struct AssistantMessageFrameError {
    message: String,
}

impl AssistantMessageFrameError {
    fn new(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
        }
    }

    /// The exact source-compatible error text.
    #[must_use]
    pub fn message(&self) -> &str {
        &self.message
    }
}

#[derive(Clone, Debug)]
enum EncoderBlockState {
    Text {
        /// Number of UTF-16 code units already present at text-start.
        covered_chars: usize,
        /// Number of UTF-16 code units accounted for in emitted deltas.
        delta_chars: usize,
    },
    Thinking {
        /// Number of UTF-16 code units already present at thinking-start.
        covered_chars: usize,
        /// Number of UTF-16 code units accounted for in emitted deltas.
        delta_chars: usize,
    },
    ToolCall {
        caught_up: bool,
        catchup_json: String,
        snapshot_arguments: String,
    },
}

/// Encodes one assistant event stream into compact replay frames.
///
/// The provider's `partial` message remains a shared live accumulator. The
/// encoder keeps per-block offsets so an event already visible in an older
/// queued snapshot is not emitted a second time. `done` and `error` mark the
/// stream terminally and produce no frame.
#[derive(Clone, Debug, Default)]
pub struct AssistantMessageFrameEncoder {
    started: bool,
    terminal: bool,
    blocks: HashMap<u64, EncoderBlockState>,
}

impl AssistantMessageFrameEncoder {
    /// Creates an encoder for a new assistant stream.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Encodes one event, accepting either an owned event or a borrowed event.
    ///
    /// `Ok(None)` is returned for terminal events and for deltas covered by a
    /// previously observed snapshot. Terminal events are never persisted as
    /// frames.
    ///
    /// # Errors
    ///
    /// Returns [`AssistantMessageFrameError`] when an event arrives out of
    /// order, references an invalid content block, or repeats a block
    /// transition.
    pub fn encode<E>(
        &mut self,
        event: E,
    ) -> Result<Option<AssistantMessageFrame>, AssistantMessageFrameError>
    where
        E: Borrow<AssistantMessageEvent>,
    {
        let event = event.borrow();
        if self.terminal {
            return Err(frame_error(format!(
                "Assistant message event {} follows a terminal event",
                event_type(event)
            )));
        }
        if !self.started
            && !matches!(
                event,
                AssistantMessageEvent::Start { .. }
                    | AssistantMessageEvent::Done { .. }
                    | AssistantMessageEvent::Error { .. }
            )
        {
            return Err(frame_error(format!(
                "Assistant message {} event appears before start",
                event_type(event)
            )));
        }

        match event {
            AssistantMessageEvent::Start { partial } => self.encode_start(partial),
            AssistantMessageEvent::Done { .. } => self.encode_done(),
            AssistantMessageEvent::Error { .. } => {
                self.terminal = true;
                Ok(None)
            }
            AssistantMessageEvent::TextStart {
                content_index,
                partial,
            } => self.encode_text_start(*content_index, partial),
            AssistantMessageEvent::TextDelta {
                content_index,
                delta,
                ..
            } => self.encode_text_delta(*content_index, delta, TextBlockKind::Text),
            AssistantMessageEvent::TextEnd {
                content_index,
                content,
                partial,
            } => self.encode_text_end(*content_index, content, partial),
            AssistantMessageEvent::ThinkingStart {
                content_index,
                partial,
            } => self.encode_thinking_start(*content_index, partial),
            AssistantMessageEvent::ThinkingDelta {
                content_index,
                delta,
                ..
            } => self.encode_text_delta(*content_index, delta, TextBlockKind::Thinking),
            AssistantMessageEvent::ThinkingEnd {
                content_index,
                content,
                partial,
            } => self.encode_thinking_end(*content_index, content, partial),
            AssistantMessageEvent::ToolCallStart {
                content_index,
                partial,
            } => self.encode_tool_call_start(*content_index, partial),
            AssistantMessageEvent::ToolCallDelta {
                content_index,
                delta,
                ..
            } => self.encode_tool_call_delta(*content_index, delta),
            AssistantMessageEvent::ToolCallEnd {
                content_index,
                tool_call,
                partial,
            } => self.encode_tool_call_end(*content_index, tool_call, partial),
        }
    }

    fn encode_start(
        &mut self,
        partial: &Arc<AssistantMessage>,
    ) -> Result<Option<AssistantMessageFrame>, AssistantMessageFrameError> {
        if self.started {
            return Err(frame_error(
                "Assistant message stream contains more than one start event",
            ));
        }
        self.started = true;
        Ok(Some(AssistantMessageFrame::Start {
            partial: Box::new(clone_start_message(partial)),
        }))
    }
    fn encode_done(&mut self) -> Result<Option<AssistantMessageFrame>, AssistantMessageFrameError> {
        if !self.started {
            return Err(frame_error(
                "Assistant message done event appears before start",
            ));
        }
        self.terminal = true;
        Ok(None)
    }

    fn encode_text_start(
        &mut self,
        content_index: u64,
        partial: &Arc<AssistantMessage>,
    ) -> Result<Option<AssistantMessageFrame>, AssistantMessageFrameError> {
        let block = event_block(partial, content_index, "text_start")?;
        let AssistantContent::Text(content) = block else {
            return Err(frame_error(format!(
                "text_start event points to {} block at index {content_index}",
                content_kind(block)
            )));
        };
        self.start_block(
            content_index,
            EncoderBlockState::Text {
                covered_chars: utf16_len(&content.text),
                delta_chars: 0,
            },
        )?;
        Ok(Some(AssistantMessageFrame::TextStart {
            content_index,
            content: content.clone(),
        }))
    }

    fn encode_text_end(
        &mut self,
        content_index: u64,
        content: &str,
        partial: &Arc<AssistantMessage>,
    ) -> Result<Option<AssistantMessageFrame>, AssistantMessageFrameError> {
        let block = event_block(partial, content_index, "text_end")?;
        let AssistantContent::Text(text) = block else {
            return Err(frame_error(format!(
                "text_end event points to {} block at index {content_index}",
                content_kind(block)
            )));
        };
        self.end_block(content_index, EncoderKind::Text)?;
        Ok(Some(AssistantMessageFrame::TextEnd {
            content_index,
            content: content.to_owned(),
            text_signature: text.text_signature.clone(),
        }))
    }

    fn encode_thinking_start(
        &mut self,
        content_index: u64,
        partial: &Arc<AssistantMessage>,
    ) -> Result<Option<AssistantMessageFrame>, AssistantMessageFrameError> {
        let block = event_block(partial, content_index, "thinking_start")?;
        let AssistantContent::Thinking(content) = block else {
            return Err(frame_error(format!(
                "thinking_start event points to {} block at index {content_index}",
                content_kind(block)
            )));
        };
        self.start_block(
            content_index,
            EncoderBlockState::Thinking {
                covered_chars: utf16_len(&content.thinking),
                delta_chars: 0,
            },
        )?;
        Ok(Some(AssistantMessageFrame::ThinkingStart {
            content_index,
            content: content.clone(),
        }))
    }

    fn encode_thinking_end(
        &mut self,
        content_index: u64,
        content: &str,
        partial: &Arc<AssistantMessage>,
    ) -> Result<Option<AssistantMessageFrame>, AssistantMessageFrameError> {
        let block = event_block(partial, content_index, "thinking_end")?;
        let AssistantContent::Thinking(thinking) = block else {
            return Err(frame_error(format!(
                "thinking_end event points to {} block at index {content_index}",
                content_kind(block)
            )));
        };
        self.end_block(content_index, EncoderKind::Thinking)?;
        Ok(Some(AssistantMessageFrame::ThinkingEnd {
            content_index,
            content: content.to_owned(),
            thinking_signature: thinking.thinking_signature.clone(),
            redacted: thinking.redacted,
        }))
    }

    fn encode_tool_call_start(
        &mut self,
        content_index: u64,
        partial: &Arc<AssistantMessage>,
    ) -> Result<Option<AssistantMessageFrame>, AssistantMessageFrameError> {
        let block = event_block(partial, content_index, "toolcall_start")?;
        let AssistantContent::ToolCall(tool_call) = block else {
            return Err(frame_error(format!(
                "toolcall_start event points to {} block at index {content_index}",
                content_kind(block)
            )));
        };
        let snapshot_arguments = serialized_arguments(&tool_call.arguments)?;
        let empty_arguments = serialized_arguments(&parse_streaming_json(""))?;
        let caught_up = snapshot_arguments == empty_arguments;
        self.start_block(
            content_index,
            EncoderBlockState::ToolCall {
                caught_up,
                catchup_json: String::new(),
                snapshot_arguments: if caught_up {
                    String::new()
                } else {
                    snapshot_arguments
                },
            },
        )?;
        Ok(Some(AssistantMessageFrame::ToolCallStart {
            content_index,
            tool_call: tool_call.clone(),
        }))
    }

    fn encode_tool_call_end(
        &mut self,
        content_index: u64,
        tool_call: &ToolCall,
        partial: &Arc<AssistantMessage>,
    ) -> Result<Option<AssistantMessageFrame>, AssistantMessageFrameError> {
        let block = event_block(partial, content_index, "toolcall_end")?;
        let AssistantContent::ToolCall(_) = block else {
            return Err(frame_error(format!(
                "toolcall_end event points to {} block at index {content_index}",
                content_kind(block)
            )));
        };
        self.end_block(content_index, EncoderKind::ToolCall)?;
        Ok(Some(AssistantMessageFrame::ToolCallEnd {
            content_index,
            id: tool_call.id.clone(),
            name: tool_call.name.clone(),
            arguments: tool_call.arguments.clone(),
            thought_signature: tool_call.thought_signature.clone(),
            namespace: tool_call.namespace.clone(),
        }))
    }

    fn start_block(
        &mut self,
        content_index: u64,
        state: EncoderBlockState,
    ) -> Result<(), AssistantMessageFrameError> {
        assert_content_index(content_index)?;
        if self.blocks.contains_key(&content_index) {
            return Err(frame_error(format!(
                "Assistant message block {content_index} starts more than once"
            )));
        }
        self.blocks.insert(content_index, state);
        Ok(())
    }

    fn block(
        &mut self,
        content_index: u64,
        kind: EncoderKind,
    ) -> Result<&mut EncoderBlockState, AssistantMessageFrameError> {
        assert_content_index(content_index)?;
        let state = self.blocks.get_mut(&content_index).ok_or_else(|| {
            frame_error(format!(
                "Assistant message {} block {} has not started",
                kind.as_str(),
                content_index
            ))
        })?;
        if state.encoder_kind() != kind {
            return Err(frame_error(format!(
                "Assistant message block {} is {}, not {}",
                content_index,
                state.encoder_kind().as_str(),
                kind.as_str()
            )));
        }
        Ok(state)
    }

    fn end_block(
        &mut self,
        content_index: u64,
        kind: EncoderKind,
    ) -> Result<(), AssistantMessageFrameError> {
        self.block(content_index, kind)?;
        self.blocks.remove(&content_index);
        Ok(())
    }

    fn encode_text_delta(
        &mut self,
        content_index: u64,
        delta: &str,
        kind: TextBlockKind,
    ) -> Result<Option<AssistantMessageFrame>, AssistantMessageFrameError> {
        let state = self.block(content_index, kind.encoder_kind())?;
        let (covered_chars, delta_chars) = match state {
            EncoderBlockState::Text {
                covered_chars,
                delta_chars,
            } if kind == TextBlockKind::Text => (covered_chars, delta_chars),
            EncoderBlockState::Thinking {
                covered_chars,
                delta_chars,
            } if kind == TextBlockKind::Thinking => (covered_chars, delta_chars),
            EncoderBlockState::Text { .. }
            | EncoderBlockState::Thinking { .. }
            | EncoderBlockState::ToolCall { .. } => {
                return Err(frame_error("Unreachable text encoder state"));
            }
        };
        let delta_start = *delta_chars;
        *delta_chars = (*delta_chars).saturating_add(utf16_len(delta));
        let covered = (*covered_chars).saturating_sub(delta_start);
        if covered >= utf16_len(delta) {
            return Ok(None);
        }
        let uncovered = utf16_suffix(delta, covered);
        Ok(Some(match kind {
            TextBlockKind::Text => AssistantMessageFrame::TextDelta {
                content_index,
                delta: uncovered,
            },
            TextBlockKind::Thinking => AssistantMessageFrame::ThinkingDelta {
                content_index,
                delta: uncovered,
            },
        }))
    }

    fn encode_tool_call_delta(
        &mut self,
        content_index: u64,
        delta: &str,
    ) -> Result<Option<AssistantMessageFrame>, AssistantMessageFrameError> {
        let state = self.block(content_index, EncoderKind::ToolCall)?;
        let EncoderBlockState::ToolCall {
            caught_up,
            catchup_json,
            snapshot_arguments,
        } = state
        else {
            return Err(frame_error("Unreachable tool-call encoder state"));
        };

        if *caught_up {
            return Ok(if delta.is_empty() {
                None
            } else {
                Some(AssistantMessageFrame::ToolCallDelta {
                    content_index,
                    delta: delta.to_owned(),
                })
            });
        }

        catchup_json.push_str(delta);
        let arguments_value = parse_streaming_json(catchup_json);
        if serialized_arguments(&arguments_value)? != *snapshot_arguments {
            let snapshot_value = parse_streaming_json(snapshot_arguments);
            if !is_json_prefix(
                &Value::Object(snapshot_value),
                &Value::Object(arguments_value),
            ) {
                return Ok(None);
            }
        }

        *caught_up = true;
        snapshot_arguments.clear();
        let json = std::mem::take(catchup_json);
        Ok(if json.is_empty() {
            None
        } else {
            Some(AssistantMessageFrame::ToolCallCheckpoint {
                content_index,
                json,
            })
        })
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum EncoderKind {
    Text,
    Thinking,
    ToolCall,
}

impl EncoderKind {
    const fn as_str(self) -> &'static str {
        match self {
            Self::Text => "text",
            Self::Thinking => "thinking",
            Self::ToolCall => "toolCall",
        }
    }
}

impl EncoderBlockState {
    const fn encoder_kind(&self) -> EncoderKind {
        match self {
            Self::Text { .. } => EncoderKind::Text,
            Self::Thinking { .. } => EncoderKind::Thinking,
            Self::ToolCall { .. } => EncoderKind::ToolCall,
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum TextBlockKind {
    Text,
    Thinking,
}

impl TextBlockKind {
    const fn encoder_kind(self) -> EncoderKind {
        match self {
            Self::Text => EncoderKind::Text,
            Self::Thinking => EncoderKind::Thinking,
        }
    }
}

#[derive(Clone, Debug)]
enum ReducerBlockState {
    Text { ended: bool },
    Thinking { ended: bool },
    ToolCall { ended: bool, json: String },
}

impl ReducerBlockState {
    const fn kind(&self) -> ReducerKind {
        match self {
            Self::Text { .. } => ReducerKind::Text,
            Self::Thinking { .. } => ReducerKind::Thinking,
            Self::ToolCall { .. } => ReducerKind::ToolCall,
        }
    }

    const fn ended(&self) -> bool {
        match self {
            Self::Text { ended } | Self::Thinking { ended } | Self::ToolCall { ended, .. } => {
                *ended
            }
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum ReducerKind {
    Text,
    Thinking,
    ToolCall,
}

impl ReducerKind {
    const fn as_str(self) -> &'static str {
        match self {
            Self::Text => "text",
            Self::Thinking => "thinking",
            Self::ToolCall => "toolCall",
        }
    }
}

/// Replays compact frames without mutating the supplied frames.
///
/// The returned `None` is the source-compatible result for an iterable that
/// contains no start frame. Frames before the start are ignored until a start
/// appears, but their first type is remembered so the eventual sequence can be
/// rejected rather than silently accepting a non-monotonic stream.
///
/// # Errors
///
/// Returns [`AssistantMessageFrameError`] when frames violate start ordering,
/// block ordering, content-index bounds, or block-kind transitions.
pub fn reduce_assistant_message_frames<I, F>(
    frames: I,
) -> Result<Option<AssistantMessage>, AssistantMessageFrameError>
where
    I: IntoIterator<Item = F>,
    F: Borrow<AssistantMessageFrame>,
{
    let mut message: Option<AssistantMessage> = None;
    let mut frame_before_start: Option<&'static str> = None;
    let mut states: HashMap<u64, ReducerBlockState> = HashMap::new();

    for frame in frames {
        let frame = frame.borrow();
        if let AssistantMessageFrame::Start { partial } = frame {
            if message.is_some() {
                return Err(frame_error(
                    "Assistant message frame sequence contains more than one start frame",
                ));
            }
            if let Some(frame_type) = frame_before_start {
                return Err(frame_error(format!(
                    "{frame_type} frame appears before the start frame"
                )));
            }
            message = Some(partial.as_ref().clone());
            continue;
        }

        if message.is_none() {
            if frame_before_start.is_none() {
                frame_before_start = Some(frame_type(frame));
            }
            continue;
        }

        let current = message
            .as_mut()
            .ok_or_else(|| frame_error("Unreachable assistant frame reducer state"))?;
        reduce_frame(frame, current, &mut states)?;
    }

    let Some(mut message) = message else {
        return Ok(None);
    };

    let unfinished = states
        .iter()
        .filter_map(|(content_index, state)| match state {
            ReducerBlockState::ToolCall { ended: false, json } if !json.is_empty() => {
                Some((*content_index, json.clone()))
            }
            _ => None,
        })
        .collect::<Vec<_>>();
    for (content_index, json) in unfinished {
        let index = checked_index(content_index)?;
        let Some(AssistantContent::ToolCall(tool_call)) = message.content.get_mut(index) else {
            return Err(frame_error("Unreachable tool-call frame state"));
        };
        tool_call.arguments = parse_streaming_json(&json);
    }

    Ok(Some(message))
}

fn reduce_frame(
    frame: &AssistantMessageFrame,
    current: &mut AssistantMessage,
    states: &mut HashMap<u64, ReducerBlockState>,
) -> Result<(), AssistantMessageFrameError> {
    match frame {
        AssistantMessageFrame::Start { .. } => Err(frame_error(
            "Assistant message frame sequence contains more than one start frame",
        )),
        AssistantMessageFrame::TextStart {
            content_index,
            content,
        } => reduce_text_start(current, states, *content_index, content),
        AssistantMessageFrame::TextDelta {
            content_index,
            delta,
        } => reduce_text_delta(current, states, *content_index, delta),
        AssistantMessageFrame::TextEnd { .. } => reduce_text_end(current, states, frame),
        AssistantMessageFrame::ThinkingStart {
            content_index,
            content,
        } => reduce_thinking_start(current, states, *content_index, content),
        AssistantMessageFrame::ThinkingDelta {
            content_index,
            delta,
        } => reduce_thinking_delta(current, states, *content_index, delta),
        AssistantMessageFrame::ThinkingEnd { .. } => reduce_thinking_end(current, states, frame),
        AssistantMessageFrame::ToolCallStart {
            content_index,
            tool_call,
        } => reduce_tool_call_start(current, states, *content_index, tool_call),
        AssistantMessageFrame::ToolCallCheckpoint {
            content_index,
            json,
        } => reduce_tool_call_checkpoint(current, states, *content_index, json),
        AssistantMessageFrame::ToolCallDelta {
            content_index,
            delta,
        } => reduce_tool_call_delta(current, states, *content_index, delta),
        AssistantMessageFrame::ToolCallEnd { .. } => reduce_tool_call_end(current, states, frame),
    }
}

fn reduce_text_start(
    current: &mut AssistantMessage,
    states: &mut HashMap<u64, ReducerBlockState>,
    content_index: u64,
    content: &TextContent,
) -> Result<(), AssistantMessageFrameError> {
    append_block(
        current,
        states,
        content_index,
        AssistantContent::Text(content.clone()),
        ReducerBlockState::Text { ended: false },
    )
}

fn reduce_text_delta(
    current: &mut AssistantMessage,
    states: &mut HashMap<u64, ReducerBlockState>,
    content_index: u64,
    delta: &str,
) -> Result<(), AssistantMessageFrameError> {
    let (block, _) = active_block(
        current,
        states,
        content_index,
        ReducerKind::Text,
        "text_delta",
    )?;
    let AssistantContent::Text(text) = block else {
        return Err(frame_error("Unreachable text frame state"));
    };
    text.text.push_str(delta);
    Ok(())
}

fn reduce_text_end(
    current: &mut AssistantMessage,
    states: &mut HashMap<u64, ReducerBlockState>,
    frame: &AssistantMessageFrame,
) -> Result<(), AssistantMessageFrameError> {
    let AssistantMessageFrame::TextEnd {
        content_index,
        content,
        text_signature,
    } = frame
    else {
        return Err(frame_error("Unreachable text reducer state"));
    };
    let (block, state) = active_block(
        current,
        states,
        *content_index,
        ReducerKind::Text,
        "text_end",
    )?;
    let AssistantContent::Text(text) = block else {
        return Err(frame_error("Unreachable text frame state"));
    };
    content.clone_into(&mut text.text);
    text.text_signature.clone_from(text_signature);
    let ReducerBlockState::Text { ended } = state else {
        return Err(frame_error("Unreachable text reducer state"));
    };
    *ended = true;
    Ok(())
}

fn reduce_thinking_start(
    current: &mut AssistantMessage,
    states: &mut HashMap<u64, ReducerBlockState>,
    content_index: u64,
    content: &ThinkingContent,
) -> Result<(), AssistantMessageFrameError> {
    append_block(
        current,
        states,
        content_index,
        AssistantContent::Thinking(content.clone()),
        ReducerBlockState::Thinking { ended: false },
    )
}

fn reduce_thinking_delta(
    current: &mut AssistantMessage,
    states: &mut HashMap<u64, ReducerBlockState>,
    content_index: u64,
    delta: &str,
) -> Result<(), AssistantMessageFrameError> {
    let (block, _) = active_block(
        current,
        states,
        content_index,
        ReducerKind::Thinking,
        "thinking_delta",
    )?;
    let AssistantContent::Thinking(thinking) = block else {
        return Err(frame_error("Unreachable thinking frame state"));
    };
    thinking.thinking.push_str(delta);
    Ok(())
}

fn reduce_thinking_end(
    current: &mut AssistantMessage,
    states: &mut HashMap<u64, ReducerBlockState>,
    frame: &AssistantMessageFrame,
) -> Result<(), AssistantMessageFrameError> {
    let AssistantMessageFrame::ThinkingEnd {
        content_index,
        content,
        thinking_signature,
        redacted,
    } = frame
    else {
        return Err(frame_error("Unreachable thinking reducer state"));
    };
    let (block, state) = active_block(
        current,
        states,
        *content_index,
        ReducerKind::Thinking,
        "thinking_end",
    )?;
    let AssistantContent::Thinking(thinking) = block else {
        return Err(frame_error("Unreachable thinking frame state"));
    };
    content.clone_into(&mut thinking.thinking);
    thinking.thinking_signature.clone_from(thinking_signature);
    thinking.redacted = *redacted;
    let ReducerBlockState::Thinking { ended } = state else {
        return Err(frame_error("Unreachable thinking reducer state"));
    };
    *ended = true;
    Ok(())
}

fn reduce_tool_call_start(
    current: &mut AssistantMessage,
    states: &mut HashMap<u64, ReducerBlockState>,
    content_index: u64,
    tool_call: &ToolCall,
) -> Result<(), AssistantMessageFrameError> {
    append_block(
        current,
        states,
        content_index,
        AssistantContent::ToolCall(tool_call.clone()),
        ReducerBlockState::ToolCall {
            ended: false,
            json: String::new(),
        },
    )
}

fn reduce_tool_call_checkpoint(
    current: &mut AssistantMessage,
    states: &mut HashMap<u64, ReducerBlockState>,
    content_index: u64,
    json: &str,
) -> Result<(), AssistantMessageFrameError> {
    let (block, state) = active_block(
        current,
        states,
        content_index,
        ReducerKind::ToolCall,
        "toolcall_checkpoint",
    )?;
    let AssistantContent::ToolCall(tool_call) = block else {
        return Err(frame_error("Unreachable tool-call checkpoint state"));
    };
    let ReducerBlockState::ToolCall {
        json: state_json, ..
    } = state
    else {
        return Err(frame_error("Unreachable tool-call checkpoint state"));
    };
    json.clone_into(state_json);
    tool_call.arguments = parse_streaming_json(json);
    Ok(())
}

fn reduce_tool_call_delta(
    current: &mut AssistantMessage,
    states: &mut HashMap<u64, ReducerBlockState>,
    content_index: u64,
    delta: &str,
) -> Result<(), AssistantMessageFrameError> {
    let (_, state) = active_block(
        current,
        states,
        content_index,
        ReducerKind::ToolCall,
        "toolcall_delta",
    )?;
    let ReducerBlockState::ToolCall { json, .. } = state else {
        return Err(frame_error("Unreachable tool-call frame state"));
    };
    json.push_str(delta);
    Ok(())
}

fn reduce_tool_call_end(
    current: &mut AssistantMessage,
    states: &mut HashMap<u64, ReducerBlockState>,
    frame: &AssistantMessageFrame,
) -> Result<(), AssistantMessageFrameError> {
    let AssistantMessageFrame::ToolCallEnd {
        content_index,
        id,
        name,
        arguments,
        thought_signature,
        namespace,
    } = frame
    else {
        return Err(frame_error("Unreachable tool-call reducer state"));
    };
    let (block, state) = active_block(
        current,
        states,
        *content_index,
        ReducerKind::ToolCall,
        "toolcall_end",
    )?;
    let AssistantContent::ToolCall(tool_call) = block else {
        return Err(frame_error("Unreachable tool-call frame state"));
    };
    tool_call.id.clone_from(id);
    tool_call.name.clone_from(name);
    tool_call.arguments.clone_from(arguments);
    tool_call.thought_signature.clone_from(thought_signature);
    tool_call.namespace.clone_from(namespace);
    let ReducerBlockState::ToolCall { ended, .. } = state else {
        return Err(frame_error("Unreachable tool-call reducer state"));
    };
    *ended = true;
    Ok(())
}

fn append_block(
    message: &mut AssistantMessage,
    states: &mut HashMap<u64, ReducerBlockState>,
    content_index: u64,
    block: AssistantContent,
    state: ReducerBlockState,
) -> Result<(), AssistantMessageFrameError> {
    let index = checked_index(content_index)?;
    if index != message.content.len() {
        let reason = if index < message.content.len() {
            "already exists"
        } else {
            "would leave a gap"
        };
        return Err(frame_error(format!(
            "Cannot start assistant message block at index {content_index}: {reason}"
        )));
    }
    message.content.push(block);
    states.insert(content_index, state);
    Ok(())
}

fn active_block<'a>(
    message: &'a mut AssistantMessage,
    states: &'a mut HashMap<u64, ReducerBlockState>,
    content_index: u64,
    expected_kind: ReducerKind,
    frame_type: &str,
) -> Result<(&'a mut AssistantContent, &'a mut ReducerBlockState), AssistantMessageFrameError> {
    let index = checked_index(content_index)?;
    let Some(state) = states.get_mut(&content_index) else {
        return Err(frame_error(format!(
            "{frame_type} frame has no started block at index {content_index}"
        )));
    };
    let Some(block) = message.content.get_mut(index) else {
        return Err(frame_error(format!(
            "{frame_type} frame has no started block at index {content_index}"
        )));
    };
    if state.kind() != expected_kind || content_kind(block) != expected_kind.as_str() {
        return Err(frame_error(format!(
            "{} frame expected {} block at index {}, found {}",
            frame_type,
            expected_kind.as_str(),
            content_index,
            content_kind(block)
        )));
    }
    if state.ended() {
        return Err(frame_error(format!(
            "{frame_type} frame follows the end of block at index {content_index}"
        )));
    }
    Ok((block, state))
}

fn clone_start_message(message: &AssistantMessage) -> AssistantMessage {
    let mut start = AssistantMessage::new(
        message.api.clone(),
        message.provider.clone(),
        message.model.clone(),
        message.timestamp,
    );
    start.response_model.clone_from(&message.response_model);
    start.response_id.clone_from(&message.response_id);
    start
        .provider_thinking_level
        .clone_from(&message.provider_thinking_level);
    start.diagnostics.clone_from(&message.diagnostics);
    start.usage.clone_from(&message.usage);
    // A frame start is a live snapshot, never a terminal Stop or Deferred
    // result. This is intentionally not inferred from the source message.
    start.stop_reason = StopReason::Pending;
    start
}

fn assert_content_index(content_index: u64) -> Result<(), AssistantMessageFrameError> {
    checked_index(content_index).map(|_| ())
}

fn checked_index(content_index: u64) -> Result<usize, AssistantMessageFrameError> {
    if content_index > MAX_SAFE_INTEGER {
        return Err(frame_error(format!(
            "Invalid assistant message frame contentIndex: {content_index}"
        )));
    }
    usize::try_from(content_index).map_err(|_| {
        frame_error(format!(
            "Invalid assistant message frame contentIndex: {content_index}"
        ))
    })
}

fn event_block<'a>(
    partial: &'a Arc<AssistantMessage>,
    content_index: u64,
    event_type: &str,
) -> Result<&'a AssistantContent, AssistantMessageFrameError> {
    let index = checked_index(content_index)?;
    partial.content.get(index).ok_or_else(|| {
        frame_error(format!(
            "{event_type} event has no content block at index {content_index}"
        ))
    })
}

fn content_kind(content: &AssistantContent) -> &'static str {
    match content {
        AssistantContent::Text(_) => "text",
        AssistantContent::Thinking(_) => "thinking",
        AssistantContent::ToolCall(_) => "toolCall",
    }
}

fn event_type(event: &AssistantMessageEvent) -> &'static str {
    match event {
        AssistantMessageEvent::Start { .. } => "start",
        AssistantMessageEvent::TextStart { .. } => "text_start",
        AssistantMessageEvent::TextDelta { .. } => "text_delta",
        AssistantMessageEvent::TextEnd { .. } => "text_end",
        AssistantMessageEvent::ThinkingStart { .. } => "thinking_start",
        AssistantMessageEvent::ThinkingDelta { .. } => "thinking_delta",
        AssistantMessageEvent::ThinkingEnd { .. } => "thinking_end",
        AssistantMessageEvent::ToolCallStart { .. } => "toolcall_start",
        AssistantMessageEvent::ToolCallDelta { .. } => "toolcall_delta",
        AssistantMessageEvent::ToolCallEnd { .. } => "toolcall_end",
        AssistantMessageEvent::Done { .. } => "done",
        AssistantMessageEvent::Error { .. } => "error",
    }
}

fn frame_type(frame: &AssistantMessageFrame) -> &'static str {
    match frame {
        AssistantMessageFrame::Start { .. } => "start",
        AssistantMessageFrame::TextStart { .. } => "text_start",
        AssistantMessageFrame::TextDelta { .. } => "text_delta",
        AssistantMessageFrame::TextEnd { .. } => "text_end",
        AssistantMessageFrame::ThinkingStart { .. } => "thinking_start",
        AssistantMessageFrame::ThinkingDelta { .. } => "thinking_delta",
        AssistantMessageFrame::ThinkingEnd { .. } => "thinking_end",
        AssistantMessageFrame::ToolCallStart { .. } => "toolcall_start",
        AssistantMessageFrame::ToolCallCheckpoint { .. } => "toolcall_checkpoint",
        AssistantMessageFrame::ToolCallDelta { .. } => "toolcall_delta",
        AssistantMessageFrame::ToolCallEnd { .. } => "toolcall_end",
    }
}

struct NormalizedJsonObject<'a>(&'a Map<String, Value>);

struct NormalizedJsonValue<'a>(&'a Value);

fn serialize_normalized_object<S>(
    object: &Map<String, Value>,
    serializer: S,
) -> Result<S::Ok, S::Error>
where
    S: serde::Serializer,
{
    let mut map = serializer.serialize_map(Some(object.len()))?;
    for (key, value) in object {
        map.serialize_entry(key, &NormalizedJsonValue(value))?;
    }
    map.end()
}

impl Serialize for NormalizedJsonObject<'_> {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        serialize_normalized_object(self.0, serializer)
    }
}

impl Serialize for NormalizedJsonValue<'_> {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        match self.0 {
            Value::Null => serializer.serialize_unit(),
            Value::Bool(value) => serializer.serialize_bool(*value),
            Value::Number(value) => {
                let value = value
                    .as_f64()
                    .filter(|value| value.is_finite())
                    .ok_or_else(|| {
                        <S::Error as serde::ser::Error>::custom(
                            "Tool-call arguments contain a non-finite number",
                        )
                    })?;
                serializer.serialize_f64(if value == 0.0 { 0.0 } else { value })
            }
            Value::String(value) => serializer.serialize_str(value),
            Value::Array(values) => {
                let mut sequence = serializer.serialize_seq(Some(values.len()))?;
                for value in values {
                    sequence.serialize_element(&NormalizedJsonValue(value))?;
                }
                sequence.end()
            }
            Value::Object(object) => serialize_normalized_object(object, serializer),
        }
    }
}

fn serialized_arguments(
    arguments: &Map<String, Value>,
) -> Result<String, AssistantMessageFrameError> {
    serde_json::to_string(&NormalizedJsonObject(arguments))
        .map_err(|_| frame_error("Tool-call arguments are not JSON-serializable"))
}

fn is_json_prefix(snapshot: &Value, current: &Value) -> bool {
    match (snapshot, current) {
        (Value::String(snapshot), Value::String(current)) => current.starts_with(snapshot),
        (Value::Array(snapshot), Value::Array(current)) => {
            snapshot.len() <= current.len()
                && snapshot
                    .iter()
                    .zip(current)
                    .all(|(snapshot, current)| is_json_prefix(snapshot, current))
        }
        (Value::Object(snapshot), Value::Object(current)) => snapshot.iter().all(|(key, value)| {
            current
                .get(key)
                .is_some_and(|current| is_json_prefix(value, current))
        }),
        (Value::Object(_) | Value::Array(_), _) | (_, Value::Object(_) | Value::Array(_)) => false,
        (Value::Number(snapshot), Value::Number(current)) => snapshot
            .as_f64()
            .zip(current.as_f64())
            .is_some_and(|(snapshot, current)| snapshot.to_bits() == current.to_bits()),
        _ => snapshot == current,
    }
}

fn utf16_len(value: &str) -> usize {
    value.encode_utf16().count()
}

/// Returns a suffix using JavaScript's UTF-16 offset semantics. Rust strings
/// cannot contain an isolated surrogate; if a source offset falls inside an
/// astral scalar, `from_utf16_lossy` makes that otherwise unrepresentable
/// boundary explicit rather than slicing at a byte offset.
fn utf16_suffix(value: &str, offset: usize) -> String {
    if offset == 0 {
        return value.to_owned();
    }
    let total = utf16_len(value);
    if offset >= total {
        return String::new();
    }

    let mut consumed = 0;
    for (byte_index, character) in value.char_indices() {
        let units = character.len_utf16();
        if consumed + units == offset {
            return value[byte_index + character.len_utf8()..].to_owned();
        }
        if consumed + units > offset {
            let suffix = value.encode_utf16().skip(offset).collect::<Vec<_>>();
            return String::from_utf16_lossy(&suffix);
        }
        consumed += units;
    }
    String::new()
}

fn frame_error(message: impl Into<String>) -> AssistantMessageFrameError {
    AssistantMessageFrameError::new(message)
}

#[cfg(test)]
#[expect(
    clippy::expect_used,
    clippy::panic,
    reason = "unit tests use expect and panic to make invariant failures explicit"
)]
mod tests {
    use super::*;
    use serde_json::json;

    fn assistant_with_content(content: Vec<AssistantContent>) -> Arc<AssistantMessage> {
        let mut message = AssistantMessage::new("api", "provider", "model", 1);
        message.content = content;
        Arc::new(message)
    }

    fn assistant_with_tool_arguments(arguments: &str) -> Arc<AssistantMessage> {
        assistant_with_content(vec![AssistantContent::ToolCall(ToolCall::new(
            "call-1",
            "search",
            parse_streaming_json(arguments),
        ))])
    }

    fn encoder_after_tool_call_start(snapshot: &str) -> AssistantMessageFrameEncoder {
        let mut encoder = AssistantMessageFrameEncoder::new();
        encoder
            .encode(AssistantMessageEvent::Start {
                partial: assistant_with_content(Vec::new()),
            })
            .expect("start should encode")
            .expect("start frame");
        encoder
            .encode(AssistantMessageEvent::ToolCallStart {
                content_index: 0,
                partial: assistant_with_tool_arguments(snapshot),
            })
            .expect("tool start should encode")
            .expect("tool start frame");
        encoder
    }

    fn encode_tool_call_delta(
        encoder: &mut AssistantMessageFrameEncoder,
        delta: &str,
        partial: &str,
    ) -> Result<Option<AssistantMessageFrame>, AssistantMessageFrameError> {
        encoder.encode(AssistantMessageEvent::ToolCallDelta {
            content_index: 0,
            delta: delta.to_owned(),
            partial: assistant_with_tool_arguments(partial),
        })
    }

    #[test]
    fn encoder_catches_up_when_integer_and_float_number_spellings_differ() {
        let mut encoder = encoder_after_tool_call_start(r#"{"n":1}"#);
        let checkpoint = encode_tool_call_delta(&mut encoder, r#"{"n":1.0}"#, r#"{"n":1.0}"#)
            .expect("equivalent number spellings should catch up")
            .expect("catch-up should emit a checkpoint");
        let delta = encode_tool_call_delta(&mut encoder, "next", r#"{"n":1.0}"#)
            .expect("delta after catch-up should encode")
            .expect("delta after catch-up should be emitted");

        assert_eq!(
            vec![checkpoint, delta],
            vec![
                AssistantMessageFrame::ToolCallCheckpoint {
                    content_index: 0,
                    json: r#"{"n":1.0}"#.into(),
                },
                AssistantMessageFrame::ToolCallDelta {
                    content_index: 0,
                    delta: "next".into(),
                },
            ]
        );
    }

    #[test]
    fn encoder_catches_up_when_float_and_integer_number_spellings_differ() {
        let mut encoder = encoder_after_tool_call_start(r#"{"n":1.0}"#);
        let checkpoint = encode_tool_call_delta(&mut encoder, r#"{"n":1}"#, r#"{"n":1}"#)
            .expect("equivalent number spellings should catch up")
            .expect("catch-up should emit a checkpoint");
        let delta = encode_tool_call_delta(&mut encoder, "next", r#"{"n":1}"#)
            .expect("delta after catch-up should encode")
            .expect("delta after catch-up should be emitted");

        assert_eq!(
            vec![checkpoint, delta],
            vec![
                AssistantMessageFrame::ToolCallCheckpoint {
                    content_index: 0,
                    json: r#"{"n":1}"#.into(),
                },
                AssistantMessageFrame::ToolCallDelta {
                    content_index: 0,
                    delta: "next".into(),
                },
            ]
        );
    }

    #[test]
    fn encoder_catches_up_when_zero_spellings_differ() {
        let mut encoder = encoder_after_tool_call_start(r#"{"n":0}"#);
        let checkpoint = encode_tool_call_delta(&mut encoder, r#"{"n":-0.0}"#, r#"{"n":-0.0}"#)
            .expect("JSON zero spellings should catch up")
            .expect("catch-up should emit a checkpoint");

        assert_eq!(
            checkpoint,
            AssistantMessageFrame::ToolCallCheckpoint {
                content_index: 0,
                json: r#"{"n":-0.0}"#.into(),
            }
        );
    }

    #[test]
    fn encoder_rejects_signed_zero_difference_when_other_values_only_prefix_match() {
        let mut encoder = encoder_after_tool_call_start(r#"{"n":0,"text":"foo"}"#);
        let result = encode_tool_call_delta(
            &mut encoder,
            r#"{"n":-0.0,"text":"foobar"}"#,
            r#"{"n":-0.0,"text":"foobar"}"#,
        )
        .expect("prefix comparison should not fail to encode");

        assert!(result.is_none());
    }

    #[test]
    fn encoder_uses_utf16_offsets_for_emoji_catchup() {
        let start_partial = assistant_with_content(Vec::new());
        let text_partial =
            assistant_with_content(vec![AssistantContent::Text(TextContent::new("😀"))]);
        let mut encoder = AssistantMessageFrameEncoder::new();
        let start = encoder
            .encode(AssistantMessageEvent::Start {
                partial: start_partial,
            })
            .expect("start should encode")
            .expect("start frame");
        let text_start = encoder
            .encode(AssistantMessageEvent::TextStart {
                content_index: 0,
                partial: text_partial.clone(),
            })
            .expect("text start should encode")
            .expect("text start frame");
        let delta = encoder
            .encode(AssistantMessageEvent::TextDelta {
                content_index: 0,
                delta: "😀x".into(),
                partial: text_partial,
            })
            .expect("delta should encode")
            .expect("uncovered delta");
        assert_eq!(
            delta,
            AssistantMessageFrame::TextDelta {
                content_index: 0,
                delta: "x".into()
            }
        );
        let reduced = reduce_assistant_message_frames([start, text_start, delta])
            .expect("frames should reduce")
            .expect("start frame should be present");
        let AssistantContent::Text(text) = &reduced.content[0] else {
            panic!("expected text block");
        };
        assert_eq!(text.text, "😀x");
    }

    #[test]
    fn tool_call_catchup_emits_checkpoint_only_after_snapshot_prefix() {
        let arguments = json!({"query": "foo"})
            .as_object()
            .cloned()
            .expect("object");
        let tool_partial = assistant_with_content(vec![AssistantContent::ToolCall(ToolCall::new(
            "call-1", "search", arguments,
        ))]);
        let mut encoder = AssistantMessageFrameEncoder::new();
        let start = encoder
            .encode(AssistantMessageEvent::Start {
                partial: assistant_with_content(Vec::new()),
            })
            .expect("start should encode")
            .expect("start frame");
        let tool_start = encoder
            .encode(AssistantMessageEvent::ToolCallStart {
                content_index: 0,
                partial: tool_partial.clone(),
            })
            .expect("tool start should encode")
            .expect("tool start frame");
        let first = encoder
            .encode(AssistantMessageEvent::ToolCallDelta {
                content_index: 0,
                delta: r#"{"query":"fo"#.into(),
                partial: tool_partial.clone(),
            })
            .expect("prefix delta should be retained");
        assert!(first.is_none());
        let checkpoint = encoder
            .encode(AssistantMessageEvent::ToolCallDelta {
                content_index: 0,
                delta: r#"obar"}"#.into(),
                partial: tool_partial,
            })
            .expect("extended prefix should catch up")
            .expect("checkpoint frame");
        assert!(matches!(
            checkpoint,
            AssistantMessageFrame::ToolCallCheckpoint { .. }
        ));
        let reduced = reduce_assistant_message_frames([start, tool_start, checkpoint])
            .expect("frames should reduce")
            .expect("start frame should be present");
        let AssistantContent::ToolCall(call) = &reduced.content[0] else {
            panic!("expected tool call");
        };
        assert_eq!(call.arguments["query"], json!("foobar"));
    }

    #[test]
    fn reducer_rejects_prestart_and_nonmonotonic_blocks() {
        let prestart = AssistantMessageFrame::TextDelta {
            content_index: 0,
            delta: "x".into(),
        };
        let start = AssistantMessageFrame::Start {
            partial: Box::new(AssistantMessage::new("api", "provider", "model", 1)),
        };
        let error = reduce_assistant_message_frames([prestart, start.clone()])
            .expect_err("prestart frame must be rejected");
        assert_eq!(
            error.message(),
            "text_delta frame appears before the start frame"
        );

        let gap = reduce_assistant_message_frames([
            start,
            AssistantMessageFrame::TextStart {
                content_index: 1,
                content: TextContent::new("gap"),
            },
        ])
        .expect_err("gapped block must be rejected");
        assert_eq!(
            gap.message(),
            "Cannot start assistant message block at index 1: would leave a gap"
        );
    }

    #[test]
    fn encoder_excludes_terminal_events_and_rejects_followers() {
        let mut encoder = AssistantMessageFrameEncoder::new();
        let start = Arc::new(AssistantMessage::new("api", "provider", "model", 1));
        encoder
            .encode(AssistantMessageEvent::Start {
                partial: start.clone(),
            })
            .expect("start should encode");
        assert!(
            encoder
                .encode(AssistantMessageEvent::Done {
                    reason: crate::types::DoneReason::Stop,
                    message: (*start).clone(),
                })
                .expect("done should terminate")
                .is_none()
        );
        let error = encoder
            .encode(AssistantMessageEvent::TextDelta {
                content_index: 0,
                delta: "late".into(),
                partial: start,
            })
            .expect_err("events after terminal must be rejected");
        assert_eq!(
            error.message(),
            "Assistant message event text_delta follows a terminal event"
        );
    }
}
