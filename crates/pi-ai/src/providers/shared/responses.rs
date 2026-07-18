//! `OpenAI` Responses message/tool conversion and response-event processing.
//!
//! This module intentionally contains no endpoint, authentication, or request
//! construction policy. Those differences remain in the owning adapters.

use std::collections::{BTreeMap, BTreeSet};

use serde_json::{Map, Value, json};

use super::{
    calculate_cost, parse_streaming_json, sanitize_surrogates, short_hash, transform_messages,
};
use crate::providers::stream_state::{AssistantState, ProviderEventSender};
use crate::types::{
    AssistantContent, AssistantMessage, Context, DoneReason, ErrorReason, Message, Model,
    ModelInput, StopReason, Tool, ToolResultContent, Usage, UsageCost, UserContent,
    UserMessageContent,
};

const NO_TOOL_OUTPUT: &str = "(no tool output)";

/// Options controlling conversion of conversation history to Responses input items.
#[derive(Clone, Debug)]
pub(crate) struct ConvertMessagesOptions {
    /// Include the context system prompt as the first input item.
    pub(crate) include_system_prompt: bool,
    /// Tools that are replayed through Responses' deferred tool-search items.
    pub(crate) deferred_tools: BTreeMap<String, Tool>,
}

impl Default for ConvertMessagesOptions {
    fn default() -> Self {
        Self {
            include_system_prompt: true,
            deferred_tools: BTreeMap::new(),
        }
    }
}

/// Options controlling conversion of native tools to Responses function tools.
#[derive(Clone, Copy, Debug)]
pub(crate) struct ConvertToolsOptions {
    /// Value of the Responses `strict` field. `None` encodes JSON null.
    pub(crate) strict: Option<bool>,
    /// Mark the tools as deferred-loading results.
    pub(crate) defer_loading: bool,
}

impl Default for ConvertToolsOptions {
    fn default() -> Self {
        Self {
            strict: Some(false),
            defer_loading: false,
        }
    }
}

/// Service-tier request policy used while finalizing Responses usage.
#[derive(Clone, Debug, Default)]
pub(crate) struct ProcessOptions {
    /// Tier requested in the request body, used when the response omits it.
    pub(crate) request_service_tier: Option<String>,
    /// Apply `OpenAI`'s flex/priority price multiplier.
    pub(crate) apply_service_tier_pricing: bool,
    /// Treat a response tier of `default` as the requested tier (Codex behavior).
    pub(crate) default_service_tier_uses_request: bool,
}

/// Error raised by response conversion or semantic event processing.
#[derive(Clone, Debug, Eq, PartialEq, thiserror::Error)]
#[error("{0}")]
pub(crate) struct ResponsesError(String);

impl ResponsesError {
    fn new(message: impl Into<String>) -> Self {
        Self(message.into())
    }
}

/// Convert native context messages to the flat `OpenAI` Responses input shape.
pub(crate) fn convert_messages(
    model: &Model,
    context: &Context,
    allowed_tool_call_providers: &BTreeSet<String>,
    options: &ConvertMessagesOptions,
) -> Vec<Value> {
    let mut normalize = |id: &str, target: &Model, source: &AssistantMessage| {
        normalize_tool_call_id(id, target, source, allowed_tool_call_providers)
    };
    let transformed = transform_messages(&context.messages, model, &mut normalize);
    let mut input = Vec::new();
    let mut loaded_tool_names = BTreeSet::new();

    if options.include_system_prompt
        && let Some(system_prompt) = context.system_prompt.as_deref()
    {
        let supports_developer = compat_bool(model, "supportsDeveloperRole", true);
        let role = if model.reasoning && supports_developer {
            "developer"
        } else {
            "system"
        };
        input.push(json!({"role": role, "content": sanitize_surrogates(system_prompt)}));
    }

    for (message_index, message) in transformed.iter().enumerate() {
        match message {
            Message::User(user) => convert_user_message(user, &mut input),
            Message::Assistant(assistant) => {
                convert_assistant_message(model, assistant, message_index, &mut input);
            }
            Message::ToolResult(result) => convert_tool_result_message(
                model,
                result,
                message_index,
                options,
                &mut loaded_tool_names,
                &mut input,
            ),
        }
    }
    input
}

fn convert_user_message(user: &crate::types::UserMessage, input: &mut Vec<Value>) {
    match &user.content {
        UserMessageContent::Text(text) => input.push(json!({
            "role": "user",
            "content": sanitize_surrogates(text),
        })),
        UserMessageContent::Blocks(blocks) => {
            let parts: Vec<Value> = blocks
                .iter()
                .map(|block| match block {
                    UserContent::Text(text) => json!({
                        "type": "input_text",
                        "text": sanitize_surrogates(&text.text),
                    }),
                    UserContent::Image(image) => json!({
                        "type": "input_image",
                        "detail": "auto",
                        "image_url": format!(
                            "data:{};base64,{}",
                            image.mime_type, image.data
                        ),
                    }),
                })
                .collect();
            if !parts.is_empty() {
                input.push(json!({"role": "user", "content": parts}));
            }
        }
    }
}

fn convert_assistant_message(
    model: &Model,
    assistant: &AssistantMessage,
    message_index: usize,
    input: &mut Vec<Value>,
) {
    let different_model = assistant.model != model.id
        && assistant.provider == model.provider
        && assistant.api == model.api;
    let mut text_block_index = 0_u64;
    for block in &assistant.content {
        match block {
            AssistantContent::Thinking(thinking) => {
                if let Some(signature) = thinking.thinking_signature.as_deref()
                    && let Ok(item) = serde_json::from_str::<Value>(signature)
                {
                    input.push(item);
                }
            }
            AssistantContent::Text(text) => {
                let parsed = text.text_signature.as_deref().map(parse_text_signature);
                let fallback = if text_block_index == 0 {
                    format!("msg_pi_{message_index}")
                } else {
                    format!("msg_pi_{message_index}_{text_block_index}")
                };
                text_block_index += 1;
                let mut id = parsed
                    .as_ref()
                    .map_or(fallback, |signature| signature.id.clone());
                if id.chars().count() > 64 {
                    id = format!("msg_{}", short_hash(&id));
                }
                let mut item = json!({
                    "type": "message",
                    "role": "assistant",
                    "content": [{
                        "type": "output_text",
                        "text": sanitize_surrogates(&text.text),
                        "annotations": [],
                    }],
                    "status": "completed",
                    "id": id,
                });
                if let Some(phase) = parsed.and_then(|signature| signature.phase) {
                    item["phase"] = Value::String(phase);
                }
                input.push(item);
            }
            AssistantContent::ToolCall(tool_call) => {
                let (call_id, mut item_id) = split_tool_id(&tool_call.id);
                if different_model && item_id.as_deref().is_some_and(|id| id.starts_with("fc_")) {
                    item_id = None;
                }
                let mut item = json!({
                    "type": "function_call",
                    "call_id": call_id,
                    "name": tool_call.name,
                    "arguments": serde_json::to_string(&tool_call.arguments)
                        .unwrap_or_else(|_| "{}".to_owned()),
                });
                if let Some(item_id) = item_id {
                    item["id"] = Value::String(item_id);
                }
                input.push(item);
            }
        }
    }
}

fn convert_tool_result_message(
    model: &Model,
    result: &crate::types::ToolResultMessage,
    message_index: usize,
    options: &ConvertMessagesOptions,
    loaded_tool_names: &mut BTreeSet<String>,
    input: &mut Vec<Value>,
) {
    let (call_id, _) = split_tool_id(&result.tool_call_id);
    let text = result
        .content
        .iter()
        .filter_map(|block| match block {
            ToolResultContent::Text(text) => Some(text.text.as_str()),
            ToolResultContent::Image(_) => None,
        })
        .collect::<Vec<_>>()
        .join("\n");
    let has_images = result
        .content
        .iter()
        .any(|block| matches!(block, ToolResultContent::Image(_)));
    let has_text = !text.is_empty();
    let output = if has_images && model.input.contains(&ModelInput::Image) {
        Value::Array(
            result
                .content
                .iter()
                .map(|block| match block {
                    ToolResultContent::Text(text) => json!({
                        "type": "input_text",
                        "text": sanitize_surrogates(&text.text),
                    }),
                    ToolResultContent::Image(image) => json!({
                        "type": "input_image",
                        "detail": "auto",
                        "image_url": format!(
                            "data:{};base64,{}",
                            image.mime_type, image.data
                        ),
                    }),
                })
                .collect(),
        )
    } else {
        Value::String(if has_text {
            sanitize_surrogates(&text).into_owned()
        } else if has_images {
            "(see attached image)".to_owned()
        } else {
            NO_TOOL_OUTPUT.to_owned()
        })
    };
    input.push(json!({
        "type": "function_call_output",
        "call_id": call_id,
        "output": output,
    }));

    let mut deferred = Vec::new();
    for name in result.added_tool_names.iter().flatten() {
        if loaded_tool_names.insert(name.clone())
            && let Some(tool) = options.deferred_tools.get(name)
        {
            deferred.push(tool.clone());
        }
    }
    if !deferred.is_empty() {
        let names = deferred
            .iter()
            .map(|tool| tool.name.as_str())
            .collect::<Vec<_>>();
        let search_call_id = format!("tool_search_{message_index}");
        input.push(json!({
            "type": "tool_search_call",
            "id": search_call_id,
            "execution": "client",
            "status": "completed",
            "arguments": {"query": names.join(" "), "limit": names.len()},
        }));
        input.push(json!({
            "type": "tool_search_output",
            "call_id": search_call_id,
            "execution": "client",
            "status": "completed",
            "tools": convert_tools(
                &deferred,
                ConvertToolsOptions { strict: Some(false), defer_loading: true },
            ),
        }));
    }
}

/// Convert native tool definitions to `OpenAI` Responses function tools.
pub(crate) fn convert_tools(tools: &[Tool], options: ConvertToolsOptions) -> Vec<Value> {
    tools
        .iter()
        .map(|tool| {
            let mut value = json!({
                "type": "function",
                "name": tool.name,
                "description": tool.description,
                "parameters": tool.parameters,
                "strict": options.strict,
            });
            if options.defer_loading {
                value["defer_loading"] = Value::Bool(true);
            }
            value
        })
        .collect()
}

/// Stateful converter from raw Responses stream events to native semantic events.
pub(crate) struct ResponsesStreamProcessor {
    model: Model,
    sender: ProviderEventSender,
    state: AssistantState,
    slots: BTreeMap<u64, OutputSlot>,
    reasoning_blocks_by_id: BTreeMap<String, u64>,
    options: ProcessOptions,
    saw_terminal: bool,
}

#[derive(Clone, Debug)]
enum OutputSlot {
    Thinking {
        content_index: u64,
    },
    Text {
        content_index: u64,
    },
    ToolCall {
        content_index: u64,
        arguments: StreamingArguments,
    },
}

#[derive(Clone, Debug, Default)]
struct StreamingArguments {
    raw: String,
    parser: IncrementalObjectParser,
}

impl StreamingArguments {
    fn from_initial(raw: &str) -> Self {
        let mut arguments = Self::default();
        arguments.push(raw);
        arguments
    }

    fn push(&mut self, fragment: &str) {
        self.raw.push_str(fragment);
        self.parser.push(fragment);
    }

    fn current(&self) -> Map<String, Value> {
        self.parser.object.clone()
    }

    fn replace_with_final(&mut self, final_json: String) -> Map<String, Value> {
        self.raw = final_json;
        let parsed = parse_streaming_json(&self.raw);
        self.parser = IncrementalObjectParser::from_object(parsed.clone());
        parsed
    }
}

#[derive(Clone, Debug, Default)]
struct IncrementalObjectParser {
    object: Map<String, Value>,
    stack: Vec<JsonContainer>,
    token: Option<JsonToken>,
    started: bool,
    rejected: bool,
}

#[derive(Clone, Debug)]
enum JsonContainer {
    Object {
        path: Vec<JsonPathPart>,
        state: ObjectState,
        key: Option<String>,
    },
    Array {
        path: Vec<JsonPathPart>,
        state: ArrayState,
    },
}

#[derive(Clone, Debug, Eq, PartialEq)]
enum JsonPathPart {
    Key(String),
    Index(usize),
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum ObjectState {
    Key,
    Colon,
    Value,
    Comma,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum ArrayState {
    Value,
    Comma,
}

#[derive(Clone, Debug)]
enum JsonToken {
    String {
        destination: StringDestination,
        value: String,
        pending_whitespace: String,
        escape: StringEscape,
    },
    Number {
        path: Vec<JsonPathPart>,
        raw: String,
    },
    Literal {
        path: Vec<JsonPathPart>,
        expected: &'static str,
        raw: String,
    },
}

#[derive(Clone, Debug)]
enum StringDestination {
    Key,
    Value(Vec<JsonPathPart>),
}

#[derive(Clone, Debug, Default)]
enum StringEscape {
    #[default]
    None,
    Slash {
        base_len: usize,
    },
    Unicode {
        base_len: usize,
        digits: String,
    },
}

impl IncrementalObjectParser {
    fn from_object(object: Map<String, Value>) -> Self {
        Self {
            object,
            started: true,
            ..Self::default()
        }
    }

    fn push(&mut self, fragment: &str) {
        if self.rejected {
            return;
        }
        for character in fragment.chars() {
            self.consume(character);
            if self.rejected {
                break;
            }
        }
    }

    fn consume(&mut self, character: char) {
        if let Some(mut token) = self.token.take()
            && self.consume_token(&mut token, character)
        {
            self.token = Some(token);
            return;
        }
        self.consume_syntax(character);
    }

    fn consume_token(&mut self, token: &mut JsonToken, character: char) -> bool {
        match token {
            JsonToken::String {
                destination,
                value,
                pending_whitespace,
                escape,
            } => {
                self.consume_string_token(destination, value, pending_whitespace, escape, character)
            }
            JsonToken::Number { path, raw } => {
                if matches!(character, '0'..='9' | '-' | '+' | '.' | 'e' | 'E') {
                    raw.push(character);
                    if let Ok(value) = serde_json::from_str::<Value>(raw) {
                        self.set_value(path, value);
                    }
                    true
                } else {
                    false
                }
            }
            JsonToken::Literal {
                path,
                expected,
                raw,
            } => {
                let next = expected.as_bytes().get(raw.len()).copied().map(char::from);
                if next == Some(character) {
                    raw.push(character);
                    if raw == expected {
                        let value = match *expected {
                            "true" => Value::Bool(true),
                            "false" => Value::Bool(false),
                            _ => Value::Null,
                        };
                        self.set_value(path, value);
                    }
                    true
                } else {
                    false
                }
            }
        }
    }

    fn consume_string_token(
        &mut self,
        destination: &StringDestination,
        value: &mut String,
        pending_whitespace: &mut String,
        escape: &mut StringEscape,
        character: char,
    ) -> bool {
        if character.is_whitespace() {
            pending_whitespace.push(character);
            return true;
        }
        value.push_str(pending_whitespace);
        pending_whitespace.clear();
        match escape {
            StringEscape::None if character == '"' => {
                if matches!(destination, StringDestination::Key)
                    && let Some(JsonContainer::Object { state, key, .. }) = self.stack.last_mut()
                {
                    *key = Some(value.clone());
                    *state = ObjectState::Colon;
                }
                return false;
            }
            StringEscape::None if character == '\\' => {
                let base_len = value.len();
                value.push('\\');
                *escape = StringEscape::Slash { base_len };
            }
            StringEscape::Slash { base_len } if character == 'u' => {
                value.push('u');
                *escape = StringEscape::Unicode {
                    base_len: *base_len,
                    digits: String::new(),
                };
            }
            StringEscape::Slash { base_len } => {
                let replacement = match character {
                    '"' => Some('"'),
                    '\\' => Some('\\'),
                    '/' => Some('/'),
                    'b' => Some('\u{0008}'),
                    'f' => Some('\u{000c}'),
                    'n' => Some('\n'),
                    'r' => Some('\r'),
                    't' => Some('\t'),
                    _ => None,
                };
                if let Some(replacement) = replacement {
                    value.truncate(*base_len);
                    value.push(replacement);
                } else {
                    value.push(character);
                }
                *escape = StringEscape::None;
            }
            StringEscape::Unicode { base_len, digits } if character.is_ascii_hexdigit() => {
                digits.push(character);
                value.push(character);
                if digits.len() == 4 {
                    if let Ok(code) = u32::from_str_radix(digits, 16)
                        && let Some(decoded) = char::from_u32(code)
                    {
                        value.truncate(*base_len);
                        value.push(decoded);
                    }
                    *escape = StringEscape::None;
                }
            }
            StringEscape::Unicode { .. } => {
                value.push(character);
                *escape = StringEscape::None;
            }
            StringEscape::None => value.push(character),
        }
        if let StringDestination::Value(path) = destination {
            self.set_value(path, Value::String(value.clone()));
        }
        true
    }

    fn consume_syntax(&mut self, character: char) {
        if !self.started {
            if character == '{' {
                self.started = true;
                self.stack.push(JsonContainer::Object {
                    path: Vec::new(),
                    state: ObjectState::Key,
                    key: None,
                });
            } else if !character.is_whitespace() {
                self.rejected = true;
            }
            return;
        }
        if character.is_whitespace() {
            return;
        }
        match self.stack.last() {
            Some(JsonContainer::Object {
                state: ObjectState::Key,
                ..
            }) => match character {
                '"' => {
                    self.token = Some(JsonToken::String {
                        destination: StringDestination::Key,
                        value: String::new(),
                        pending_whitespace: String::new(),
                        escape: StringEscape::None,
                    });
                }
                '}' => {
                    let _closed = self.stack.pop();
                }
                _ => {}
            },
            Some(JsonContainer::Object {
                state: ObjectState::Colon,
                ..
            }) => {
                if character == ':'
                    && let Some(JsonContainer::Object { state, .. }) = self.stack.last_mut()
                {
                    *state = ObjectState::Value;
                }
            }
            Some(
                JsonContainer::Object {
                    state: ObjectState::Value,
                    ..
                }
                | JsonContainer::Array {
                    state: ArrayState::Value,
                    ..
                },
            ) => {
                self.start_value(character);
            }
            Some(JsonContainer::Object {
                state: ObjectState::Comma,
                ..
            }) => match character {
                ',' => {
                    if let Some(JsonContainer::Object { state, key, .. }) = self.stack.last_mut() {
                        *state = ObjectState::Key;
                        *key = None;
                    }
                }
                '}' => {
                    let _closed = self.stack.pop();
                }
                _ => {}
            },
            Some(JsonContainer::Array {
                state: ArrayState::Comma,
                ..
            }) => match character {
                ',' => {
                    if let Some(JsonContainer::Array { state, .. }) = self.stack.last_mut() {
                        *state = ArrayState::Value;
                    }
                }
                ']' => {
                    let _closed = self.stack.pop();
                }
                _ => {}
            },
            None => {}
        }
    }

    fn start_value(&mut self, character: char) {
        let Some(path) = self.next_value_path() else {
            return;
        };
        match character {
            '"' => {
                self.set_value(&path, Value::String(String::new()));
                self.token = Some(JsonToken::String {
                    destination: StringDestination::Value(path),
                    value: String::new(),
                    pending_whitespace: String::new(),
                    escape: StringEscape::None,
                });
            }
            '{' => {
                self.set_value(&path, Value::Object(Map::new()));
                self.stack.push(JsonContainer::Object {
                    path,
                    state: ObjectState::Key,
                    key: None,
                });
            }
            '[' => {
                self.set_value(&path, Value::Array(Vec::new()));
                self.stack.push(JsonContainer::Array {
                    path,
                    state: ArrayState::Value,
                });
            }
            't' => {
                self.token = Some(JsonToken::Literal {
                    path,
                    expected: "true",
                    raw: "t".into(),
                });
            }
            'f' => {
                self.token = Some(JsonToken::Literal {
                    path,
                    expected: "false",
                    raw: "f".into(),
                });
            }
            'n' => {
                self.token = Some(JsonToken::Literal {
                    path,
                    expected: "null",
                    raw: "n".into(),
                });
            }
            '-' | '0'..='9' => {
                let raw = character.to_string();
                if let Ok(value) = serde_json::from_str::<Value>(&raw) {
                    self.set_value(&path, value);
                }
                self.token = Some(JsonToken::Number { path, raw });
            }
            ']' => {
                let _closed = self.stack.pop();
            }
            _ => {}
        }
    }

    fn next_value_path(&mut self) -> Option<Vec<JsonPathPart>> {
        match self.stack.last_mut()? {
            JsonContainer::Object { path, state, key } => {
                let key = key.take()?;
                *state = ObjectState::Comma;
                let mut value_path = path.clone();
                value_path.push(JsonPathPart::Key(key));
                Some(value_path)
            }
            JsonContainer::Array { path, state } => {
                *state = ArrayState::Comma;
                let mut value_path = path.clone();
                let index = value_at_path(&self.object, path)
                    .and_then(Value::as_array)
                    .map_or(0, Vec::len);
                value_path.push(JsonPathPart::Index(index));
                Some(value_path)
            }
        }
    }

    fn set_value(&mut self, path: &[JsonPathPart], value: Value) {
        let Some((last, parents)) = path.split_last() else {
            return;
        };
        if parents.is_empty() {
            if let JsonPathPart::Key(key) = last {
                self.object.insert(key.clone(), value);
            }
            return;
        }
        let Some(parent) = value_at_path_mut(&mut self.object, parents) else {
            return;
        };
        match (parent, last) {
            (Value::Object(object), JsonPathPart::Key(key)) => {
                object.insert(key.clone(), value);
            }
            (Value::Array(array), JsonPathPart::Index(index)) if *index == array.len() => {
                array.push(value);
            }
            (Value::Array(array), JsonPathPart::Index(index)) if *index < array.len() => {
                array[*index] = value;
            }
            _ => {}
        }
    }
}

fn value_at_path<'a>(object: &'a Map<String, Value>, path: &[JsonPathPart]) -> Option<&'a Value> {
    let (first, rest) = path.split_first()?;
    let JsonPathPart::Key(key) = first else {
        return None;
    };
    let mut value = object.get(key)?;
    for part in rest {
        value = match part {
            JsonPathPart::Key(key) => value.as_object()?.get(key)?,
            JsonPathPart::Index(index) => value.as_array()?.get(*index)?,
        };
    }
    Some(value)
}

fn value_at_path_mut<'a>(
    object: &'a mut Map<String, Value>,
    path: &[JsonPathPart],
) -> Option<&'a mut Value> {
    let (first, rest) = path.split_first()?;
    let JsonPathPart::Key(key) = first else {
        return None;
    };
    let mut value = object.get_mut(key)?;
    for part in rest {
        value = match part {
            JsonPathPart::Key(key) => value.as_object_mut()?.get_mut(key)?,
            JsonPathPart::Index(index) => value.as_array_mut()?.get_mut(*index)?,
        };
    }
    Some(value)
}

impl ResponsesStreamProcessor {
    /// Create a processor for one response stream.
    pub(crate) fn new(
        model: Model,
        message: AssistantMessage,
        sender: ProviderEventSender,
        options: ProcessOptions,
    ) -> Self {
        Self {
            model,
            sender,
            state: AssistantState::new(message),
            slots: BTreeMap::new(),
            reasoning_blocks_by_id: BTreeMap::new(),
            options,
            saw_terminal: false,
        }
    }

    /// Emit the required start event.
    pub(crate) async fn start(&self) -> Result<(), ResponsesError> {
        self.sender
            .start(self.state.snapshot())
            .await
            .map_err(|error| ResponsesError::new(error.to_string()))
    }

    /// Process one decoded Responses event. Returns true after terminal delivery.
    pub(crate) async fn handle(&mut self, event: Value) -> Result<bool, ResponsesError> {
        let event_type = event
            .get("type")
            .and_then(Value::as_str)
            .ok_or_else(|| ResponsesError::new("OpenAI Responses event is missing type"))?;
        match event_type {
            "response.created" => {
                if let Some(id) = event.pointer("/response/id").and_then(Value::as_str) {
                    self.update_message(|message| message.response_id = Some(id.to_owned()));
                }
            }
            "response.output_item.added" => self.create_slot(&event).await?,
            "response.reasoning_summary_text.delta" | "response.reasoning_text.delta" => {
                self.thinking_delta(
                    &event,
                    event.get("delta").and_then(Value::as_str).unwrap_or(""),
                )
                .await?;
            }
            "response.reasoning_summary_part.done" => {
                self.thinking_delta(&event, "\n\n").await?;
            }
            "response.output_text.delta" | "response.refusal.delta" => {
                self.text_delta(
                    &event,
                    event.get("delta").and_then(Value::as_str).unwrap_or(""),
                )
                .await?;
            }
            "response.function_call_arguments.delta" => self.tool_delta(&event).await?,
            "response.function_call_arguments.done" => self.tool_arguments_done(&event).await?,
            "response.output_item.done" => self.output_item_done(&event).await?,
            "response.completed" | "response.incomplete" | "response.failed" => {
                self.saw_terminal = true;
                let response = event.get("response").unwrap_or(&Value::Null);
                match self.finalize_response(response, event_type) {
                    Ok(reason) => {
                        self.sender
                            .done(reason, self.state.snapshot())
                            .await
                            .map_err(|error| ResponsesError::new(error.to_string()))?;
                    }
                    Err(failure) => {
                        self.fail(failure.reason, failure.message).await?;
                    }
                }
                return Ok(true);
            }
            "error" => {
                return Err(ResponsesError::new(format!(
                    "Error Code {}: {}",
                    event
                        .get("code")
                        .and_then(Value::as_str)
                        .unwrap_or("unknown"),
                    event
                        .get("message")
                        .and_then(Value::as_str)
                        .unwrap_or("Unknown error")
                )));
            }
            _ => {}
        }
        Ok(false)
    }

    /// Reject EOF unless a Responses terminal event was observed.
    pub(crate) fn finish(&self) -> Result<(), ResponsesError> {
        if self.saw_terminal {
            Ok(())
        } else {
            Err(ResponsesError::new(
                "OpenAI Responses stream ended before a terminal response event",
            ))
        }
    }

    /// Emit the sole semantic error terminal for this stream.
    pub(crate) async fn fail(
        &mut self,
        reason: ErrorReason,
        message: impl Into<String>,
    ) -> Result<(), ResponsesError> {
        let message = message.into();
        let final_message = self.state.fail(reason, message);
        self.sender
            .error(reason, final_message)
            .await
            .map_err(|error| ResponsesError::new(error.to_string()))
    }

    /// Clone the current canonical assistant message.
    pub(crate) fn message(&self) -> AssistantMessage {
        self.state.snapshot()
    }

    async fn create_slot(&mut self, event: &Value) -> Result<(), ResponsesError> {
        let Some(output_index) = output_index(event) else {
            return Ok(());
        };
        if self.slots.contains_key(&output_index) {
            return Ok(());
        }
        let item = event.get("item").unwrap_or(&Value::Null);
        let slot = match item.get("type").and_then(Value::as_str) {
            Some("reasoning") => {
                let semantic = self.state.start_thinking().map_err(state_error)?;
                let content_index = event_content_index(&semantic)?;
                self.sender.event(semantic).await.map_err(send_error)?;
                OutputSlot::Thinking { content_index }
            }
            Some("message") => {
                let semantic = self.state.start_text().map_err(state_error)?;
                let content_index = event_content_index(&semantic)?;
                self.sender.event(semantic).await.map_err(send_error)?;
                OutputSlot::Text { content_index }
            }
            Some("function_call") => {
                let call_id = item.get("call_id").and_then(Value::as_str).unwrap_or("");
                let item_id = item.get("id").and_then(Value::as_str).unwrap_or("");
                let name = item.get("name").and_then(Value::as_str).unwrap_or("");
                let semantic = self
                    .state
                    .start_tool_call(format!("{call_id}|{item_id}"), name)
                    .map_err(state_error)?;
                let content_index = event_content_index(&semantic)?;
                self.sender.event(semantic).await.map_err(send_error)?;
                OutputSlot::ToolCall {
                    content_index,
                    arguments: StreamingArguments::from_initial(
                        item.get("arguments").and_then(Value::as_str).unwrap_or(""),
                    ),
                }
            }
            _ => return Ok(()),
        };
        self.slots.insert(output_index, slot);
        Ok(())
    }

    async fn thinking_delta(&mut self, event: &Value, delta: &str) -> Result<(), ResponsesError> {
        let Some(OutputSlot::Thinking { content_index }) = self.slot(event).cloned() else {
            return Ok(());
        };
        let semantic = self
            .state
            .thinking_delta(content_index, delta)
            .map_err(state_error)?;
        self.sender.event(semantic).await.map_err(send_error)
    }

    async fn text_delta(&mut self, event: &Value, delta: &str) -> Result<(), ResponsesError> {
        let Some(OutputSlot::Text { content_index }) = self.slot(event).cloned() else {
            return Ok(());
        };
        let semantic = self
            .state
            .text_delta(content_index, delta)
            .map_err(state_error)?;
        self.sender.event(semantic).await.map_err(send_error)
    }

    async fn tool_delta(&mut self, event: &Value) -> Result<(), ResponsesError> {
        let delta = event.get("delta").and_then(Value::as_str).unwrap_or("");
        let Some(output_index) = output_index(event) else {
            return Ok(());
        };
        let Some(OutputSlot::ToolCall {
            content_index,
            arguments,
        }) = self.slots.get_mut(&output_index)
        else {
            return Ok(());
        };
        arguments.push(delta);
        let content_index = *content_index;
        let parsed = arguments.current();
        self.set_tool_arguments(content_index, parsed)?;
        let semantic = self
            .state
            .tool_call_delta(content_index, delta)
            .map_err(state_error)?;
        self.sender.event(semantic).await.map_err(send_error)
    }

    async fn tool_arguments_done(&mut self, event: &Value) -> Result<(), ResponsesError> {
        let Some(output_index) = output_index(event) else {
            return Ok(());
        };
        let final_arguments = event
            .get("arguments")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_owned();
        let Some(OutputSlot::ToolCall {
            content_index,
            arguments,
        }) = self.slots.get_mut(&output_index)
        else {
            return Ok(());
        };
        let content_index = *content_index;
        let previous = arguments.raw.clone();
        let parsed = arguments.replace_with_final(final_arguments.clone());
        self.set_tool_arguments(content_index, parsed)?;
        if let Some(delta) = final_arguments
            .strip_prefix(&previous)
            .filter(|delta| !delta.is_empty())
        {
            let semantic = self
                .state
                .tool_call_delta(content_index, delta)
                .map_err(state_error)?;
            self.sender.event(semantic).await.map_err(send_error)?;
        }
        Ok(())
    }

    async fn output_item_done(&mut self, event: &Value) -> Result<(), ResponsesError> {
        let Some(output_index) = output_index(event) else {
            return Ok(());
        };
        let item = event.get("item").unwrap_or(&Value::Null);
        if !self.slots.contains_key(&output_index) {
            self.create_slot(event).await?;
        }
        let Some(slot) = self.slots.remove(&output_index) else {
            return Ok(());
        };
        match slot {
            OutputSlot::Thinking { content_index }
                if item.get("type").and_then(Value::as_str) == Some("reasoning") =>
            {
                let summary = joined_text(item.get("summary"));
                let content = joined_text(item.get("content"));
                let final_text = if summary.is_empty() { content } else { summary };
                let signature = serde_json::to_string(item).unwrap_or_else(|_| "{}".to_owned());
                self.update_content(content_index, |block| {
                    if let AssistantContent::Thinking(thinking) = block {
                        thinking.thinking = final_text;
                        thinking.thinking_signature = Some(signature);
                    }
                })?;
                if let Some(id) = item.get("id").and_then(Value::as_str) {
                    self.reasoning_blocks_by_id
                        .insert(id.to_owned(), content_index);
                }
                let semantic = self
                    .state
                    .end_thinking(content_index)
                    .map_err(state_error)?;
                self.sender.event(semantic).await.map_err(send_error)?;
            }
            OutputSlot::Text { content_index }
                if item.get("type").and_then(Value::as_str) == Some("message") =>
            {
                let final_text = joined_output_text(item.get("content"));
                let id = item.get("id").and_then(Value::as_str).unwrap_or("");
                let phase = item.get("phase").and_then(Value::as_str);
                let signature = encode_text_signature(id, phase);
                self.update_content(content_index, |block| {
                    if let AssistantContent::Text(text) = block {
                        text.text = final_text;
                        text.text_signature = Some(signature);
                    }
                })?;
                let semantic = self.state.end_text(content_index).map_err(state_error)?;
                self.sender.event(semantic).await.map_err(send_error)?;
            }
            OutputSlot::ToolCall {
                content_index,
                arguments,
            } if item.get("type").and_then(Value::as_str) == Some("function_call") => {
                let final_json = item
                    .get("arguments")
                    .and_then(Value::as_str)
                    .unwrap_or(&arguments.raw);
                let semantic = self
                    .state
                    .end_tool_call(content_index, parse_streaming_json(final_json))
                    .map_err(state_error)?;
                self.sender.event(semantic).await.map_err(send_error)?;
            }
            _ => {}
        }
        Ok(())
    }

    fn finalize_response(
        &mut self,
        response: &Value,
        event_type: &str,
    ) -> Result<DoneReason, TerminalFailure> {
        self.backfill_reasoning(response);
        let usage = response.get("usage").unwrap_or(&Value::Null);
        let input_details = usage.get("input_tokens_details").unwrap_or(&Value::Null);
        let output_details = usage.get("output_tokens_details").unwrap_or(&Value::Null);
        let cache_read = u64_field(input_details, "cached_tokens");
        let cache_write = u64_field(input_details, "cache_write_tokens");
        let input_tokens = u64_field(usage, "input_tokens");
        let mut converted = Usage {
            input: input_tokens
                .saturating_sub(cache_read)
                .saturating_sub(cache_write),
            output: u64_field(usage, "output_tokens"),
            cache_read,
            cache_write,
            cache_write1h: None,
            reasoning: Some(u64_field(output_details, "reasoning_tokens")),
            total_tokens: u64_field(usage, "total_tokens"),
            cost: UsageCost::default(),
        };
        calculate_cost(&self.model, &mut converted);
        if self.options.apply_service_tier_pricing {
            let response_tier = response.get("service_tier").and_then(Value::as_str);
            let tier = if response_tier == Some("default")
                && self.options.default_service_tier_uses_request
            {
                self.options.request_service_tier.as_deref()
            } else {
                response_tier.or(self.options.request_service_tier.as_deref())
            };
            apply_service_tier_pricing(&self.model, &mut converted, tier);
        }
        let status = response.get("status").and_then(Value::as_str);
        let mut stop_reason = map_stop_reason(status);
        if stop_reason == StopReason::Stop
            && self
                .state
                .message()
                .content
                .iter()
                .any(|block| matches!(block, AssistantContent::ToolCall(_)))
        {
            stop_reason = StopReason::ToolUse;
        }
        let response_id = response
            .get("id")
            .and_then(Value::as_str)
            .map(str::to_owned);
        self.update_message(|message| {
            if response_id.is_some() {
                message.response_id = response_id;
            }
            message.usage = converted;
        });
        if let Some(failure) = terminal_failure(response, event_type) {
            return Err(failure);
        }
        let reason = done_reason(stop_reason);
        let final_message = self.state.finish(reason);
        self.state = AssistantState::new(final_message);
        Ok(reason)
    }

    fn backfill_reasoning(&mut self, response: &Value) {
        let Some(items) = response.get("output").and_then(Value::as_array) else {
            return;
        };
        for item in items {
            if item.get("type").and_then(Value::as_str) != Some("reasoning") {
                continue;
            }
            let Some(encrypted) = item
                .get("encrypted_content")
                .filter(|value| !value.is_null())
            else {
                continue;
            };
            let Some(id) = item.get("id").and_then(Value::as_str) else {
                continue;
            };
            let Some(content_index) = self.reasoning_blocks_by_id.get(id).copied() else {
                continue;
            };
            let encrypted = encrypted.clone();
            let _updated = self.update_content(content_index, |block| {
                let AssistantContent::Thinking(thinking) = block else {
                    return;
                };
                let Some(signature) = thinking.thinking_signature.as_deref() else {
                    return;
                };
                let Ok(mut stored) = serde_json::from_str::<Value>(signature) else {
                    return;
                };
                if stored
                    .get("encrypted_content")
                    .is_some_and(|value| !value.is_null())
                {
                    return;
                }
                stored["encrypted_content"] = encrypted;
                thinking.thinking_signature = serde_json::to_string(&stored).ok();
            });
        }
    }

    fn slot(&self, event: &Value) -> Option<&OutputSlot> {
        self.slots.get(&output_index(event)?)
    }

    fn set_tool_arguments(
        &mut self,
        content_index: u64,
        arguments: Map<String, Value>,
    ) -> Result<(), ResponsesError> {
        self.update_content(content_index, |block| {
            if let AssistantContent::ToolCall(tool_call) = block {
                tool_call.arguments = arguments;
            }
        })
    }

    fn update_content(
        &mut self,
        content_index: u64,
        update: impl FnOnce(&mut AssistantContent),
    ) -> Result<(), ResponsesError> {
        let index = usize::try_from(content_index)
            .map_err(|_| ResponsesError::new("response content index overflow"))?;
        let mut message = self.state.snapshot();
        let block = message.content.get_mut(index).ok_or_else(|| {
            ResponsesError::new(format!(
                "response content block {content_index} does not exist"
            ))
        })?;
        update(block);
        self.state = AssistantState::new(message);
        Ok(())
    }

    fn update_message(&mut self, update: impl FnOnce(&mut AssistantMessage)) {
        let mut message = self.state.snapshot();
        update(&mut message);
        self.state = AssistantState::new(message);
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct TerminalFailure {
    reason: ErrorReason,
    message: String,
}

fn terminal_failure(response: &Value, event_type: &str) -> Option<TerminalFailure> {
    let status = response.get("status").and_then(Value::as_str);
    let reason = match status {
        Some("failed") => ErrorReason::Error,
        Some("cancelled") => ErrorReason::Aborted,
        None if event_type == "response.failed" => ErrorReason::Error,
        _ => return None,
    };
    let error = response.get("error").unwrap_or(&Value::Null);
    let details = response.get("incomplete_details").unwrap_or(&Value::Null);
    let message = error
        .get("message")
        .and_then(Value::as_str)
        .or_else(|| details.get("reason").and_then(Value::as_str))
        .unwrap_or(match reason {
            ErrorReason::Aborted => "Request was aborted",
            ErrorReason::Error => "Unknown error details in response",
        })
        .to_owned();
    Some(TerminalFailure { reason, message })
}

fn normalize_tool_call_id(
    id: &str,
    target: &Model,
    source: &AssistantMessage,
    allowed: &BTreeSet<String>,
) -> String {
    if !allowed.contains(&target.provider) || !id.contains('|') {
        return normalize_id_part(id);
    }
    let (call_id, item_id) = id.split_once('|').unwrap_or((id, ""));
    let call_id = normalize_id_part(call_id);
    let foreign = source.provider != target.provider || source.api != target.api;
    let mut item_id = if foreign {
        normalize_id_part(&format!("fc_{}", short_hash(item_id)))
    } else {
        normalize_id_part(item_id)
    };
    if !item_id.starts_with("fc_") {
        item_id = normalize_id_part(&format!("fc_{item_id}"));
    }
    format!("{call_id}|{item_id}")
}

fn normalize_id_part(value: &str) -> String {
    value
        .chars()
        .map(|character| {
            if character.is_ascii_alphanumeric() || matches!(character, '_' | '-') {
                character
            } else {
                '_'
            }
        })
        .take(64)
        .collect()
}

fn split_tool_id(id: &str) -> (String, Option<String>) {
    id.split_once('|').map_or_else(
        || (id.to_owned(), None),
        |(call, item)| (call.to_owned(), Some(item.to_owned())),
    )
}

#[derive(Clone, Debug)]
struct ParsedTextSignature {
    id: String,
    phase: Option<String>,
}

fn parse_text_signature(signature: &str) -> ParsedTextSignature {
    if signature.starts_with('{')
        && let Ok(value) = serde_json::from_str::<Value>(signature)
        && value.get("v").and_then(Value::as_u64) == Some(1)
        && let Some(id) = value.get("id").and_then(Value::as_str)
    {
        let phase = value
            .get("phase")
            .and_then(Value::as_str)
            .filter(|phase| matches!(*phase, "commentary" | "final_answer"))
            .map(str::to_owned);
        return ParsedTextSignature {
            id: id.to_owned(),
            phase,
        };
    }
    ParsedTextSignature {
        id: signature.to_owned(),
        phase: None,
    }
}

fn encode_text_signature(id: &str, phase: Option<&str>) -> String {
    let mut value = json!({"v": 1, "id": id});
    if matches!(phase, Some("commentary" | "final_answer")) {
        value["phase"] = Value::String(phase.unwrap_or_default().to_owned());
    }
    serde_json::to_string(&value).unwrap_or_else(|_| format!("{{\"v\":1,\"id\":{id:?}}}"))
}

fn output_index(event: &Value) -> Option<u64> {
    event.get("output_index").and_then(Value::as_u64)
}

fn event_content_index(event: &crate::types::AssistantMessageEvent) -> Result<u64, ResponsesError> {
    serde_json::to_value(event)
        .ok()
        .and_then(|value| value.get("contentIndex").and_then(Value::as_u64))
        .ok_or_else(|| ResponsesError::new("provider event missing content index"))
}

fn joined_text(value: Option<&Value>) -> String {
    value
        .and_then(Value::as_array)
        .map(|parts| {
            parts
                .iter()
                .filter_map(|part| part.get("text").and_then(Value::as_str))
                .collect::<Vec<_>>()
                .join("\n\n")
        })
        .unwrap_or_default()
}

fn joined_output_text(value: Option<&Value>) -> String {
    value
        .and_then(Value::as_array)
        .map(|parts| {
            parts
                .iter()
                .filter_map(|part| {
                    part.get("text")
                        .and_then(Value::as_str)
                        .or_else(|| part.get("refusal").and_then(Value::as_str))
                })
                .collect::<String>()
        })
        .unwrap_or_default()
}

fn u64_field(value: &Value, name: &str) -> u64 {
    value.get(name).and_then(Value::as_u64).unwrap_or(0)
}

fn map_stop_reason(status: Option<&str>) -> StopReason {
    match status {
        Some("incomplete") => StopReason::Length,
        Some("completed" | "in_progress" | "queued") | None => StopReason::Stop,
        Some(_) => StopReason::Error,
    }
}

fn done_reason(reason: StopReason) -> DoneReason {
    match reason {
        StopReason::Length => DoneReason::Length,
        StopReason::ToolUse => DoneReason::ToolUse,
        _ => DoneReason::Stop,
    }
}

fn apply_service_tier_pricing(model: &Model, usage: &mut Usage, tier: Option<&str>) {
    let multiplier = match tier {
        Some("flex") => 0.5,
        Some("priority") if model.id == "gpt-5.5" => 2.5,
        Some("priority") => 2.0,
        _ => 1.0,
    };
    usage.cost.input *= multiplier;
    usage.cost.output *= multiplier;
    usage.cost.cache_read *= multiplier;
    usage.cost.cache_write *= multiplier;
    usage.cost.total =
        usage.cost.input + usage.cost.output + usage.cost.cache_read + usage.cost.cache_write;
}

fn compat_bool(model: &Model, field: &str, default: bool) -> bool {
    model
        .compat
        .as_ref()
        .and_then(|compat| compat.get(field))
        .and_then(Value::as_bool)
        .unwrap_or(default)
}

fn send_error(error: impl std::fmt::Display) -> ResponsesError {
    ResponsesError::new(error.to_string())
}

fn state_error(error: impl std::fmt::Display) -> ResponsesError {
    ResponsesError::new(error.to_string())
}

#[cfg(test)]
mod tests {
    use std::num::NonZeroUsize;

    use futures::StreamExt;
    use serde_json::json;

    use super::*;
    use crate::types::{DoneReason, ModelCost, ModelInput, StopReason};

    fn model() -> Model {
        Model {
            id: "gpt-5".into(),
            name: "GPT-5".into(),
            api: "openai-responses".into(),
            provider: "openai".into(),
            base_url: "https://api.openai.com/v1".into(),
            reasoning: true,
            thinking_level_map: None,
            input: vec![ModelInput::Text, ModelInput::Image],
            cost: ModelCost::default(),
            context_window: 128_000,
            max_tokens: 16_384,
            headers: None,
            compat: None,
            extra: BTreeMap::new(),
        }
    }

    fn event_capacity(capacity: usize) -> NonZeroUsize {
        NonZeroUsize::new(capacity).unwrap_or(NonZeroUsize::MIN)
    }

    #[test]
    fn foreign_pipe_ids_are_safe_and_response_tools_are_flat() {
        let source = AssistantMessage::new("openai-responses", "github-copilot", "gpt-5", 1);
        let allowed = ["openai".to_owned()].into_iter().collect();
        let normalized = normalize_tool_call_id("call bad|item bad", &model(), &source, &allowed);
        let (call_id, item_id) = normalized.split_once('|').unwrap_or(("", ""));
        assert_eq!(call_id, "call_bad");
        assert!(item_id.starts_with("fc_"));
        assert!(item_id.len() <= 64);

        let tools = convert_tools(
            &[Tool {
                name: "read".into(),
                description: "Read".into(),
                parameters: json!({"type":"object"}),
            }],
            ConvertToolsOptions::default(),
        );
        assert_eq!(tools[0]["name"], "read");
        assert_eq!(tools[0]["strict"], false);
        assert!(tools[0].get("function").is_none());
    }

    #[test]
    fn incremental_argument_parser_preserves_partial_wire_state() {
        for complete in [
            r#"{"path":"src/lib.rs","line":12}"#,
            r#"{"outer":{"items":[1,2,{"ok":true}]},"none":null}"#,
            "{\"text\":\"line\nbreak\",\"bad\":\"x\\q\"}",
            "{\"escaped_control\":\"x\\\ny\"}",
            r#"{"unicode":"\u263a","fraction":-12.5e2}"#,
            r#"{"name":"par"#,
            r#"{"ok":1,"drop":"#,
            r#"{"text":"tail\"#,
            r#"{"outer":{"items":[1,2"#,
            r#"[1,{"not":"an object root"}]"#,
        ] {
            let mut parser = StreamingArguments::default();
            let mut prefix = String::new();
            for character in complete.chars() {
                let fragment = character.to_string();
                prefix.push(character);
                parser.push(&fragment);
                assert_eq!(
                    parser.current(),
                    parse_streaming_json(&prefix),
                    "partial argument state diverged at {prefix:?}"
                );
            }
        }
    }

    #[test]
    fn incremental_argument_parser_consumes_many_tiny_fragments() {
        const PAYLOAD_BYTES: usize = 256 * 1024;
        let mut parser = StreamingArguments::default();
        parser.push(r#"{"payload":""#);
        for _ in 0..PAYLOAD_BYTES {
            parser.push("x");
        }
        parser.push(r#""}"#);
        assert_eq!(
            parser
                .current()
                .get("payload")
                .and_then(Value::as_str)
                .map(str::len),
            Some(PAYLOAD_BYTES)
        );
    }

    #[tokio::test]
    async fn partial_json_is_cleaned_and_terminal_usage_is_exact() -> Result<(), String> {
        let (sender, mut stream) = ProviderEventSender::channel(event_capacity(16));
        let message = AssistantMessage::new("openai-responses", "openai", "gpt-5", 1);
        let mut processor =
            ResponsesStreamProcessor::new(model(), message, sender, ProcessOptions::default());
        processor.start().await.map_err(|error| error.to_string())?;
        processor
            .handle(json!({"type":"response.output_item.added","output_index":0,"item":{"type":"function_call","id":"fc_1","call_id":"call_1","name":"read","arguments":""}}))
            .await
            .map_err(|error| error.to_string())?;
        processor
            .handle(json!({"type":"response.function_call_arguments.delta","output_index":0,"delta":"{\"path\":"}))
            .await
            .map_err(|error| error.to_string())?;
        processor
            .handle(json!({"type":"response.output_item.done","output_index":0,"item":{"type":"function_call","id":"fc_1","call_id":"call_1","name":"read","arguments":"{\"path\":\"x\"}"}}))
            .await
            .map_err(|error| error.to_string())?;
        let terminal = processor
            .handle(json!({"type":"response.completed","response":{"id":"resp_1","status":"completed","usage":{"input_tokens":10,"output_tokens":4,"total_tokens":14,"input_tokens_details":{"cached_tokens":3,"cache_write_tokens":2},"output_tokens_details":{"reasoning_tokens":1}},"output":[]}}))
            .await
            .map_err(|error| error.to_string())?;
        assert!(terminal);
        processor.finish().map_err(|error| error.to_string())?;
        drop(processor);

        let mut events = Vec::new();
        while let Some(event) = stream.next().await {
            events.push(event.map_err(|error| error.to_string())?);
        }
        let final_message = events.iter().find_map(|event| match event {
            crate::types::AssistantMessageEvent::Done { reason, message } => {
                assert_eq!(*reason, DoneReason::ToolUse);
                assert_eq!(message.stop_reason, StopReason::ToolUse);
                assert_eq!(message.error_message, None);
                Some(message)
            }
            _ => None,
        });
        let Some(final_message) = final_message else {
            return Err("done event missing".into());
        };
        assert_eq!(final_message.usage.input, 5);
        assert_eq!(final_message.usage.cache_read, 3);
        assert_eq!(final_message.usage.cache_write, 2);
        assert_eq!(final_message.usage.total_tokens, 14);
        let encoded = serde_json::to_value(final_message).map_err(|error| error.to_string())?;
        assert!(encoded.to_string().find("partialJson").is_none());
        Ok(())
    }

    #[tokio::test]
    async fn eof_without_terminal_is_rejected() -> Result<(), String> {
        let (sender, _stream) = ProviderEventSender::channel(event_capacity(2));
        let processor = ResponsesStreamProcessor::new(
            model(),
            AssistantMessage::new("openai-responses", "openai", "gpt-5", 1),
            sender,
            ProcessOptions::default(),
        );
        processor.start().await.map_err(|error| error.to_string())?;
        let error = match processor.finish() {
            Ok(()) => return Err("eof without terminal must fail".into()),
            Err(error) => error,
        };
        assert_eq!(
            error.to_string(),
            "OpenAI Responses stream ended before a terminal response event"
        );
        Ok(())
    }

    #[tokio::test]
    async fn failed_response_emits_exact_error_fields() -> Result<(), String> {
        let events = terminal_events(
            "response.completed",
            json!({
                "id":"resp_err",
                "status":"failed",
                "error":{"message":"provider rejected"},
                "usage":{"input_tokens":7,"output_tokens":2,"total_tokens":9}
            }),
        )
        .await?;

        assert!(
            events
                .iter()
                .all(|event| !matches!(event, crate::types::AssistantMessageEvent::Done { .. }))
        );
        let Some(crate::types::AssistantMessageEvent::Error { reason, error }) = events.last()
        else {
            return Err("expected terminal error event".into());
        };
        assert_eq!(*reason, ErrorReason::Error);
        assert_eq!(error.stop_reason, StopReason::Error);
        assert_eq!(error.error_message.as_deref(), Some("provider rejected"));
        assert_eq!(error.response_id.as_deref(), Some("resp_err"));
        assert_eq!(error.usage.input, 7);
        assert_eq!(error.usage.output, 2);
        assert!(matches!(
            error.content.as_slice(),
            [AssistantContent::Text(text)] if text.text == "partial"
        ));
        Ok(())
    }

    #[tokio::test]
    async fn cancelled_response_emits_aborted_with_provider_details() -> Result<(), String> {
        let events = terminal_events(
            "response.incomplete",
            json!({
                "id":"resp_cancel",
                "status":"cancelled",
                "incomplete_details":{"reason":"cancelled by caller"},
                "usage":{"input_tokens":5,"output_tokens":1,"total_tokens":6}
            }),
        )
        .await?;

        assert!(
            events
                .iter()
                .all(|event| !matches!(event, crate::types::AssistantMessageEvent::Done { .. }))
        );
        let Some(crate::types::AssistantMessageEvent::Error { reason, error }) = events.last()
        else {
            return Err("expected terminal aborted event".into());
        };
        assert_eq!(*reason, ErrorReason::Aborted);
        assert_eq!(error.stop_reason, StopReason::Aborted);
        assert_eq!(error.error_message.as_deref(), Some("cancelled by caller"));
        assert_eq!(error.response_id.as_deref(), Some("resp_cancel"));
        assert_eq!(error.usage.total_tokens, 6);
        assert!(matches!(
            error.content.as_slice(),
            [AssistantContent::Text(text)] if text.text == "partial"
        ));
        Ok(())
    }

    #[tokio::test]
    async fn incomplete_response_remains_a_successful_length_terminal() -> Result<(), String> {
        let events = terminal_events(
            "response.incomplete",
            json!({
                "id":"resp_length",
                "status":"incomplete",
                "incomplete_details":{"reason":"max_output_tokens"},
                "usage":{"input_tokens":5,"output_tokens":2,"total_tokens":7}
            }),
        )
        .await?;

        let Some(crate::types::AssistantMessageEvent::Done { reason, message }) = events.last()
        else {
            return Err("expected terminal done event".into());
        };
        assert_eq!(*reason, DoneReason::Length);
        assert_eq!(message.stop_reason, StopReason::Length);
        assert_eq!(message.error_message, None);
        assert_eq!(message.response_id.as_deref(), Some("resp_length"));
        Ok(())
    }

    async fn terminal_events(
        event_type: &str,
        response: Value,
    ) -> Result<Vec<crate::types::AssistantMessageEvent>, String> {
        let (sender, mut stream) = ProviderEventSender::channel(event_capacity(8));
        let mut processor = ResponsesStreamProcessor::new(
            model(),
            AssistantMessage::new("openai-responses", "openai", "gpt-5", 1),
            sender,
            ProcessOptions::default(),
        );
        processor.start().await.map_err(|error| error.to_string())?;
        processor
            .handle(json!({
                "type":"response.output_item.added",
                "output_index":0,
                "item":{"type":"message","id":"msg_1","content":[]}
            }))
            .await
            .map_err(|error| error.to_string())?;
        processor
            .handle(json!({
                "type":"response.output_text.delta",
                "output_index":0,
                "delta":"partial"
            }))
            .await
            .map_err(|error| error.to_string())?;
        let terminal = processor
            .handle(json!({"type":event_type,"response":response}))
            .await
            .map_err(|error| error.to_string())?;
        assert!(terminal);
        processor.finish().map_err(|error| error.to_string())?;
        drop(processor);

        let mut events = Vec::new();
        while let Some(event) = stream.next().await {
            events.push(event.map_err(|error| error.to_string())?);
        }
        let [
            crate::types::AssistantMessageEvent::Start { partial: start },
            crate::types::AssistantMessageEvent::TextStart {
                content_index: 0,
                partial: text_start,
            },
            crate::types::AssistantMessageEvent::TextDelta {
                content_index: 0,
                delta,
                partial: text_delta,
            },
            _terminal,
        ] = events.as_slice()
        else {
            return Err(format!("unexpected emitted event sequence: {events:?}"));
        };
        assert!(start.content.is_empty());
        assert!(matches!(
            text_start.content.as_slice(),
            [AssistantContent::Text(text)] if text.text.is_empty()
        ));
        assert_eq!(delta, "partial");
        assert!(matches!(
            text_delta.content.as_slice(),
            [AssistantContent::Text(text)] if text.text == "partial"
        ));
        Ok(events)
    }
}
