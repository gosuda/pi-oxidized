//! Pure helpers shared by native provider adapters.

pub(crate) mod cloudflare;
pub(crate) mod google;
pub(crate) mod responses;

use std::borrow::Cow;
use std::collections::BTreeMap;
use std::collections::BTreeSet;
use std::time::{SystemTime, UNIX_EPOCH};

use serde_json::{Map, Value};

use crate::types::{
    AssistantContent, AssistantMessage, Message, Model, ModelCostRates, ModelInput, StopReason,
    TextContent, ToolCall, ToolResultContent, ToolResultMessage, Usage, UsageCost, UserContent,
    UserMessageContent,
};

/// Maximum number of Unicode scalar values retained from a provider error body.
pub(crate) const MAX_PROVIDER_ERROR_BODY_CHARS: usize = 4_000;

/// Parse a complete or truncated streaming JSON object without panicking.
///
/// Invalid string escapes and raw controls are repaired before balanced suffixes
/// are added. Non-object roots and unrecoverable input return an empty object.
pub(crate) fn parse_streaming_json(input: &str) -> Map<String, Value> {
    let input = input.trim();
    if input.is_empty() {
        return Map::new();
    }

    let repaired = repair_json(input);
    // Repair raw controls and invalid escapes first so truncation cannot hide
    // characters that only become valid after escaping.
    for source in [repaired.as_str(), input] {
        if let Some(object) = parse_partial_object(source) {
            return object;
        }
    }
    Map::new()
}

fn parse_partial_object(input: &str) -> Option<Map<String, Value>> {
    let mut candidate = input.to_owned();
    loop {
        let completed = complete_json(&candidate);
        if let Ok(Value::Object(object)) = serde_json::from_str::<Value>(&completed) {
            return Some(object);
        }
        candidate.pop()?;
    }
}

fn repair_json(input: &str) -> String {
    let characters = input.chars().collect::<Vec<_>>();
    let mut repaired = String::with_capacity(input.len());
    let mut index = 0;
    let mut in_string = false;
    while let Some(&character) = characters.get(index) {
        if !in_string {
            repaired.push(character);
            in_string = character == '"';
            index += 1;
            continue;
        }
        if character == '"' {
            repaired.push(character);
            in_string = false;
            index += 1;
            continue;
        }
        if character == '\\' {
            let Some(&next) = characters.get(index + 1) else {
                repaired.push_str("\\\\");
                index += 1;
                continue;
            };
            let valid_unicode = next == 'u'
                && characters
                    .get(index + 2..index + 6)
                    .is_some_and(|digits| digits.iter().all(char::is_ascii_hexdigit));
            if valid_unicode {
                repaired.extend(characters[index..index + 6].iter());
                index += 6;
                continue;
            }
            if matches!(next, '"' | '\\' | '/' | 'b' | 'f' | 'n' | 'r' | 't') {
                repaired.push('\\');
                repaired.push(next);
                index += 2;
                continue;
            }
            repaired.push_str("\\\\");
            index += 1;
            continue;
        }
        match character {
            '\u{0008}' => repaired.push_str("\\b"),
            '\u{000c}' => repaired.push_str("\\f"),
            '\n' => repaired.push_str("\\n"),
            '\r' => repaired.push_str("\\r"),
            '\t' => repaired.push_str("\\t"),
            control if control <= '\u{001f}' => {
                let code = u32::from(control);
                repaired.push_str("\\u00");
                repaired.push(char::from_digit(code >> 4, 16).unwrap_or('0'));
                repaired.push(char::from_digit(code & 0xf, 16).unwrap_or('0'));
            }
            _ => repaired.push(character),
        }
        index += 1;
    }
    repaired
}

fn complete_json(input: &str) -> String {
    let mut output = String::with_capacity(input.len() + 8);
    let mut stack = Vec::new();
    let mut in_string = false;
    let mut escaped = false;

    for character in input.chars() {
        output.push(character);
        if in_string {
            if escaped {
                escaped = false;
            } else if character == '\\' {
                escaped = true;
            } else if character == '"' {
                in_string = false;
            }
            continue;
        }
        match character {
            '"' => in_string = true,
            '{' => stack.push('}'),
            '[' => stack.push(']'),
            '}' | ']' if stack.last() == Some(&character) => {
                let _closing = stack.pop();
            }
            _ => {}
        }
    }
    if escaped {
        let _escape = output.pop();
    }
    if in_string {
        output.push('"');
    }
    while let Some(closing) = stack.pop() {
        output.push(closing);
    }
    output
}

/// Compute pi's exact 64-bit two-lane TypeScript short hash.
///
/// Iteration uses UTF-16 code units to match `String.charCodeAt`, including for
/// non-BMP characters, and each lane uses JavaScript `Math.imul` wrapping.
pub(crate) fn short_hash(value: &str) -> String {
    let mut h1 = 0xdead_beef_u32;
    let mut h2 = 0x41c6_ce57_u32;
    for code_unit in value.encode_utf16() {
        let code_unit = u32::from(code_unit);
        h1 = (h1 ^ code_unit).wrapping_mul(2_654_435_761);
        h2 = (h2 ^ code_unit).wrapping_mul(1_597_334_677);
    }
    h1 = (h1 ^ (h1 >> 16)).wrapping_mul(2_246_822_507)
        ^ (h2 ^ (h2 >> 13)).wrapping_mul(3_266_489_909);
    h2 = (h2 ^ (h2 >> 16)).wrapping_mul(2_246_822_507)
        ^ (h1 ^ (h1 >> 13)).wrapping_mul(3_266_489_909);
    format!("{}{}", base36(h2), base36(h1))
}

fn base36(mut value: u32) -> String {
    if value == 0 {
        return "0".to_owned();
    }
    let mut reversed = Vec::new();
    while value != 0 {
        let digit = u8::try_from(value % 36).unwrap_or_default();
        reversed.push(if digit < 10 {
            char::from(b'0' + digit)
        } else {
            char::from(b'a' + digit - 10)
        });
        value /= 36;
    }
    reversed.into_iter().rev().collect()
}

/// Return provider text known to contain only valid Unicode scalar values.
///
/// Rust strings cannot contain the unpaired UTF-16 surrogates accepted by
/// JavaScript strings, so native decoded text is already sanitized. The
/// borrowed result makes that invariant explicit without allocating.
pub(crate) fn sanitize_surrogates(text: &str) -> Cow<'_, str> {
    Cow::Borrowed(text)
}

/// Apply the model's highest matching request-wide tier and calculate usage cost.
// Provider usage counters are JSON-safe integers; casting matches TypeScript's
// IEEE-754 cost calculation at the compatibility boundary.
pub(crate) fn calculate_cost(model: &Model, usage: &mut Usage) -> UsageCost {
    let input_tokens = usage
        .input
        .saturating_add(usage.cache_read)
        .saturating_add(usage.cache_write);
    let mut rates = ModelCostRates {
        input: model.cost.input,
        output: model.cost.output,
        cache_read: model.cost.cache_read,
        cache_write: model.cost.cache_write,
    };
    let mut matched_threshold = None;
    for tier in model.cost.tiers.as_deref().unwrap_or_default() {
        if input_tokens > tier.input_tokens_above
            && matched_threshold.is_none_or(|threshold| tier.input_tokens_above > threshold)
        {
            rates = ModelCostRates {
                input: tier.input,
                output: tier.output,
                cache_read: tier.cache_read,
                cache_write: tier.cache_write,
            };
            matched_threshold = Some(tier.input_tokens_above);
        }
    }

    let long_write = usage.cache_write1h.unwrap_or(0).min(usage.cache_write);
    let short_write = usage.cache_write - long_write;
    usage.cost.input = rates.input / 1_000_000.0 * tokens_as_f64(usage.input);
    usage.cost.output = rates.output / 1_000_000.0 * tokens_as_f64(usage.output);
    usage.cost.cache_read = rates.cache_read / 1_000_000.0 * tokens_as_f64(usage.cache_read);
    usage.cost.cache_write = (rates.cache_write * tokens_as_f64(short_write)
        + rates.input * 2.0 * tokens_as_f64(long_write))
        / 1_000_000.0;
    usage.cost.total =
        usage.cost.input + usage.cost.output + usage.cost.cache_read + usage.cost.cache_write;
    usage.cost.clone()
}

/// Convert a token counter to `f64` without silent mantissa loss beyond `u32::MAX`.
///
/// Practical provider usage stays well below that bound; larger values clamp so the
/// conversion remains a lossless `u32` → `f64` cast that Clippy accepts.
fn tokens_as_f64(tokens: u64) -> f64 {
    match u32::try_from(tokens) {
        Ok(tokens) => f64::from(tokens),
        Err(_) => f64::from(u32::MAX),
    }
}

/// Trim and bound a provider response body, preserving its omitted length.
pub(crate) fn truncate_error_body(body: &str) -> String {
    let body = body.trim();
    let count = body.chars().count();
    if count <= MAX_PROVIDER_ERROR_BODY_CHARS {
        return body.to_owned();
    }
    let kept: String = body.chars().take(MAX_PROVIDER_ERROR_BODY_CHARS).collect();
    format!(
        "{kept}... [truncated {} chars]",
        count - MAX_PROVIDER_ERROR_BODY_CHARS
    )
}

/// Normalize history for a target model while preserving tool-call/result pairing.
pub(crate) fn transform_messages<F>(
    messages: &[Message],
    model: &Model,
    mut normalize_tool_call_id: F,
) -> Vec<Message>
where
    F: FnMut(&str, &Model, &AssistantMessage) -> String,
{
    let mut id_map = BTreeMap::new();
    let mut transformed = Vec::with_capacity(messages.len());
    for message in messages {
        transformed.push(match message {
            Message::User(user) => {
                let mut user = user.clone();
                if !model.input.contains(&ModelInput::Image)
                    && let UserMessageContent::Blocks(content) = &user.content
                {
                    user.content = UserMessageContent::Blocks(replace_user_images(content));
                }
                Message::User(user)
            }
            Message::Assistant(assistant) => Message::Assistant(transform_assistant(
                assistant,
                model,
                &mut id_map,
                &mut normalize_tool_call_id,
            )),
            Message::ToolResult(result) => {
                let mut result = result.clone();
                if let Some(normalized) = id_map.get(&result.tool_call_id) {
                    result.tool_call_id.clone_from(normalized);
                }
                if !model.input.contains(&ModelInput::Image) {
                    result.content = replace_tool_images(&result.content);
                }
                Message::ToolResult(result)
            }
        });
    }

    repair_tool_result_sequence(transformed)
}

fn transform_assistant<F>(
    assistant: &AssistantMessage,
    model: &Model,
    id_map: &mut BTreeMap<String, String>,
    normalize_tool_call_id: &mut F,
) -> AssistantMessage
where
    F: FnMut(&str, &Model, &AssistantMessage) -> String,
{
    let same_model = assistant.provider == model.provider
        && assistant.api == model.api
        && assistant.model == model.id;
    let mut transformed = assistant.clone();
    transformed.content.clear();

    for block in &assistant.content {
        match block {
            AssistantContent::Text(text) if same_model => {
                transformed
                    .content
                    .push(AssistantContent::Text(text.clone()));
            }
            AssistantContent::Text(text) => transformed
                .content
                .push(AssistantContent::Text(TextContent::new(&text.text))),
            AssistantContent::Thinking(thinking) if thinking.redacted == Some(true) => {
                if same_model {
                    transformed
                        .content
                        .push(AssistantContent::Thinking(thinking.clone()));
                }
            }
            AssistantContent::Thinking(thinking)
                if same_model && thinking.thinking_signature.is_some() =>
            {
                transformed
                    .content
                    .push(AssistantContent::Thinking(thinking.clone()));
            }
            AssistantContent::Thinking(thinking) if thinking.thinking.trim().is_empty() => {}
            AssistantContent::Thinking(thinking) if same_model => transformed
                .content
                .push(AssistantContent::Thinking(thinking.clone())),
            AssistantContent::Thinking(thinking) => transformed
                .content
                .push(AssistantContent::Text(TextContent::new(&thinking.thinking))),
            AssistantContent::ToolCall(tool_call) => {
                let mut tool_call = tool_call.clone();
                if !same_model {
                    tool_call.thought_signature = None;
                    let normalized = normalize_tool_call_id(&tool_call.id, model, assistant);
                    if normalized != tool_call.id {
                        id_map.insert(tool_call.id.clone(), normalized.clone());
                        tool_call.id = normalized;
                    }
                }
                transformed
                    .content
                    .push(AssistantContent::ToolCall(tool_call));
            }
        }
    }
    transformed
}

fn replace_user_images(content: &[UserContent]) -> Vec<UserContent> {
    const PLACEHOLDER: &str = "(image omitted: model does not support images)";
    let mut result = Vec::with_capacity(content.len());
    let mut previous_was_placeholder = false;
    for block in content {
        match block {
            UserContent::Image(_) => {
                if !previous_was_placeholder {
                    result.push(UserContent::Text(TextContent::new(PLACEHOLDER)));
                }
                previous_was_placeholder = true;
            }
            UserContent::Text(text) => {
                previous_was_placeholder = text.text == PLACEHOLDER;
                result.push(UserContent::Text(text.clone()));
            }
        }
    }
    result
}

fn replace_tool_images(content: &[ToolResultContent]) -> Vec<ToolResultContent> {
    const PLACEHOLDER: &str = "(tool image omitted: model does not support images)";
    let mut result = Vec::with_capacity(content.len());
    let mut previous_was_placeholder = false;
    for block in content {
        match block {
            ToolResultContent::Image(_) => {
                if !previous_was_placeholder {
                    result.push(ToolResultContent::Text(TextContent::new(PLACEHOLDER)));
                }
                previous_was_placeholder = true;
            }
            ToolResultContent::Text(text) => {
                previous_was_placeholder = text.text == PLACEHOLDER;
                result.push(ToolResultContent::Text(text.clone()));
            }
        }
    }
    result
}

fn repair_tool_result_sequence(messages: Vec<Message>) -> Vec<Message> {
    let mut result = Vec::with_capacity(messages.len());
    let mut pending = Vec::new();
    let mut result_ids = BTreeSet::new();
    for message in messages {
        match message {
            Message::Assistant(assistant) => {
                append_missing_tool_results(&mut result, &mut pending, &result_ids);
                result_ids.clear();
                if matches!(
                    assistant.stop_reason,
                    StopReason::Error | StopReason::Aborted
                ) {
                    continue;
                }
                pending = assistant
                    .content
                    .iter()
                    .filter_map(|block| match block {
                        AssistantContent::ToolCall(tool_call) => Some(tool_call.clone()),
                        _ => None,
                    })
                    .collect();
                result.push(Message::Assistant(assistant));
            }
            Message::ToolResult(tool_result) => {
                result_ids.insert(tool_result.tool_call_id.clone());
                result.push(Message::ToolResult(tool_result));
            }
            Message::User(user) => {
                append_missing_tool_results(&mut result, &mut pending, &result_ids);
                result_ids.clear();
                result.push(Message::User(user));
            }
        }
    }
    append_missing_tool_results(&mut result, &mut pending, &result_ids);
    result
}

fn append_missing_tool_results(
    result: &mut Vec<Message>,
    pending: &mut Vec<ToolCall>,
    result_ids: &BTreeSet<String>,
) {
    for tool_call in pending.drain(..) {
        if result_ids.contains(&tool_call.id) {
            continue;
        }
        result.push(Message::ToolResult(ToolResultMessage::new(
            tool_call.id,
            tool_call.name,
            vec![ToolResultContent::Text(TextContent::new(
                "No result provided",
            ))],
            true,
            unix_millis(),
        )));
    }
}

fn unix_millis() -> i64 {
    match SystemTime::now().duration_since(UNIX_EPOCH) {
        Ok(duration) => i64::try_from(duration.as_millis()).unwrap_or(i64::MAX),
        Err(_) => 0,
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;
    use crate::types::{
        AssistantMessage, ModelCost, ModelCostTier, ModelInput, StopReason, TextContent, ToolCall,
        ToolResultContent,
    };

    fn model() -> Model {
        Model {
            id: "model".into(),
            name: "Model".into(),
            api: "api".into(),
            provider: "provider".into(),
            base_url: "https://example.invalid".into(),
            reasoning: false,
            thinking_level_map: None,
            input: vec![ModelInput::Text],
            cost: ModelCost {
                input: 1.0,
                output: 2.0,
                cache_read: 0.5,
                cache_write: 1.25,
                tiers: Some(vec![ModelCostTier {
                    input: 2.0,
                    output: 4.0,
                    cache_read: 1.0,
                    cache_write: 2.5,
                    input_tokens_above: 100,
                }]),
            },
            context_window: 10_000,
            max_tokens: 2_000,
            headers: None,
            compat: None,
            extra: BTreeMap::new(),
        }
    }

    fn assert_cost(actual: f64, expected: f64) {
        assert!(
            (actual - expected).abs() <= 1e-12,
            "expected {expected}, got {actual}"
        );
    }

    #[test]
    fn calculate_cost_applies_highest_tier_and_1h_write_pricing() {
        use crate::types::Usage;

        // 3400 counted input tokens clear the above-100 tier: rates 2/4/1/2.5.
        let model = model();
        let mut usage = Usage {
            input: 1000,
            output: 500,
            cache_read: 2000,
            cache_write: 400,
            cache_write1h: Some(100),
            ..Usage::default()
        };
        let cost = calculate_cost(&model, &mut usage);
        assert_cost(cost.input, 0.002);
        assert_cost(cost.output, 0.002);
        assert_cost(cost.cache_read, 0.002);
        assert_cost(cost.cache_write, 0.001_15);
        assert_cost(cost.total, 0.007_15);

        // Exactly at the threshold the tier does not apply (strict above).
        let mut boundary = Usage {
            input: 100,
            ..Usage::default()
        };
        let cost = calculate_cost(&model, &mut boundary);
        assert_cost(cost.input, 0.000_1);
        assert_cost(cost.total, 0.000_1);

        // 1h writes beyond total writes clamp to total: intentional hardening,
        // the reference would price a negative short-write leg instead.
        let mut over = Usage {
            input: 1000,
            cache_write: 400,
            cache_write1h: Some(500),
            ..Usage::default()
        };
        let cost = calculate_cost(&model, &mut over);
        assert_cost(cost.cache_write, 0.001_6);
    }
    #[test]
    fn streaming_json_returns_only_objects_and_repairs_partial_values() {
        assert_eq!(parse_streaming_json(""), Map::new());
        assert_eq!(parse_streaming_json("[1,2]"), Map::new());
        assert_eq!(
            parse_streaming_json("{\"name\":\"par"),
            json!({"name": "par"})
                .as_object()
                .cloned()
                .unwrap_or_default()
        );
        assert_eq!(
            parse_streaming_json("{\"ok\":1,\"drop\":"),
            json!({"ok": 1}).as_object().cloned().unwrap_or_default()
        );
        assert_eq!(
            parse_streaming_json("{\"text\":\"line\nbreak\"}"),
            json!({"text": "line\nbreak"})
                .as_object()
                .cloned()
                .unwrap_or_default()
        );
        assert_eq!(
            parse_streaming_json(r#"{"text":"bad\q"}"#),
            json!({"text": "bad\\q"})
                .as_object()
                .cloned()
                .unwrap_or_default()
        );
        assert_eq!(
            parse_streaming_json("{\"text\":\"tail\\"),
            json!({"text": "tail\\"})
                .as_object()
                .cloned()
                .unwrap_or_default()
        );
        assert_eq!(
            parse_streaming_json("{\"outer\":{\"items\":[1,2"),
            json!({"outer": {"items": [1, 2]}})
                .as_object()
                .cloned()
                .unwrap_or_default()
        );
    }

    #[test]
    fn short_hash_matches_typescript_utf16_behavior() {
        assert_eq!(short_hash(""), "k4n83c7h0j2b");
        assert_eq!(short_hash("hello"), "1h6qa0qrowduu");
        assert_eq!(short_hash("😀"), "13wj7r7usi372");
        assert_eq!(sanitize_surrogates("valid 😀"), "valid 😀");
    }

    #[test]
    fn cost_uses_strict_highest_tier_and_long_cache_write_formula() {
        let model = model();
        let mut boundary = Usage {
            input: 100,
            ..Usage::default()
        };
        assert!((calculate_cost(&model, &mut boundary).input - 0.0001).abs() < f64::EPSILON);

        let mut tiered = Usage {
            input: 100,
            cache_write: 2,
            cache_write1h: Some(1),
            ..Usage::default()
        };
        let cost = calculate_cost(&model, &mut tiered);
        assert!((cost.input - 0.0002).abs() < f64::EPSILON);
        assert!((cost.cache_write - 0.000_006_5).abs() < f64::EPSILON);
        assert!((cost.total - 0.000_206_5).abs() < f64::EPSILON);
    }

    #[test]
    fn error_body_truncation_is_bounded() {
        let long = "x".repeat(MAX_PROVIDER_ERROR_BODY_CHARS + 2);
        let formatted = truncate_error_body(&long);
        assert!(formatted.ends_with("[truncated 2 chars]"));
    }

    #[test]
    fn message_transform_preserves_tool_pairing_and_repairs_orphans() -> Result<(), &'static str> {
        let mut assistant = AssistantMessage::new("source-api", "source-provider", "source", 1);
        assistant.stop_reason = StopReason::ToolUse;
        let mut tool_call = ToolCall::new("raw|id", "read", Map::new());
        tool_call.thought_signature = Some("source-only".into());
        assistant
            .content
            .push(AssistantContent::ToolCall(tool_call));
        let matching = ToolResultMessage::new(
            "raw|id",
            "read",
            vec![ToolResultContent::Text(TextContent::new("ok"))],
            false,
            2,
        );
        let mut orphan = AssistantMessage::new("api", "provider", "model", 3);
        orphan
            .content
            .push(AssistantContent::ToolCall(ToolCall::new(
                "orphan",
                "write",
                Map::new(),
            )));
        orphan.stop_reason = StopReason::ToolUse;
        let transformed = transform_messages(
            &[
                Message::Assistant(assistant),
                Message::ToolResult(matching),
                Message::Assistant(orphan),
            ],
            &model(),
            |_id, _model, _source| "normalized".into(),
        );

        let Message::Assistant(assistant) = &transformed[0] else {
            return Err("first message is not assistant");
        };
        let AssistantContent::ToolCall(call) = &assistant.content[0] else {
            return Err("assistant content is not a tool call");
        };
        assert_eq!(call.id, "normalized");
        assert!(call.thought_signature.is_none());

        let Message::ToolResult(matching) = &transformed[1] else {
            return Err("second message is not tool result");
        };
        assert_eq!(matching.tool_call_id, "normalized");

        let Message::Assistant(orphan) = &transformed[2] else {
            return Err("third message is not assistant");
        };
        let AssistantContent::ToolCall(orphan_call) = &orphan.content[0] else {
            return Err("orphan content is not a tool call");
        };
        let Message::ToolResult(synthetic) = &transformed[3] else {
            return Err("missing synthetic tool result");
        };
        assert_eq!(synthetic.tool_call_id, orphan_call.id);
        assert!(synthetic.is_error);
        Ok(())
    }
}
