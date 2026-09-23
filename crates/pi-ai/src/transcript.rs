//! Transcript replay for system prompt and tool state.
//!
//! Port of `.references/pi/packages/ai/src/utils/transcript.ts` and
//! `.references/pi/packages/ai/src/utils/text.ts` system helpers.

use indexmap::IndexMap;

use crate::types::{Context, Message, SystemMessage, SystemMessageContent, Tool, ToolReference};

/// Normalized native request input with shorthand folded in.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct TranscriptContext {
    /// Ordered history with leading system message when shorthand exists.
    pub messages: Vec<Message>,
}

/// Split tool declarations between request field and in-place additions.
#[derive(Clone, Debug, PartialEq)]
pub struct TranscriptTools {
    /// Tools sent in the top-level request field.
    pub request_tools: Vec<Tool>,
    /// True when later system messages carry their own additions.
    pub anchors_additions: bool,
}

/// Added and removed tool declarations between two tool states.
#[derive(Clone, Debug, PartialEq)]
pub struct ToolStateChanges {
    /// Current tools absent or changed from previous state.
    pub tools_added: Vec<Tool>,
    /// Previous tools absent or changed in current state.
    pub tools_removed: Vec<ToolReference>,
}

/// Extract text from system content blocks.
fn system_content_text(content: &SystemMessageContent) -> String {
    match content {
        SystemMessageContent::Text(text) => text.clone(),
        SystemMessageContent::Blocks(blocks) => blocks
            .iter()
            .map(|block| block.text.as_str())
            .collect::<Vec<_>>()
            .join("\n"),
    }
}

/// Render a system message as a complete prompt.
#[must_use]
pub fn get_system_message_text(message: &SystemMessage) -> String {
    let mut parts = vec![system_content_text(&message.content)];
    if let Some(sections) = message.sections.as_ref() {
        for value in sections.values().flatten() {
            parts.push(value.clone());
        }
    }
    parts
        .into_iter()
        .filter(|part| !part.is_empty())
        .collect::<Vec<_>>()
        .join("\n\n")
}

/// Render a later system message for transports that accept mid-conversation updates.
#[must_use]
pub fn render_system_message_update(message: &SystemMessage) -> String {
    let mut parts = Vec::new();
    let text = system_content_text(&message.content);
    if !text.is_empty() {
        parts.push(text);
    }
    if let Some(sections) = message.sections.as_ref() {
        for (name, value) in sections {
            match value {
                None => parts.push(format!("Removed system prompt section \"{name}\".")),
                Some(value) => {
                    parts.push(format!(
                        "Updated system prompt section \"{name}\":\n\n{value}"
                    ));
                }
            }
        }
    }
    parts.join("\n\n")
}

/// Build the leading system message for a prompt and tool set.
#[must_use]
pub fn create_initial_system_message(
    system_prompt: Option<String>,
    tools: Option<Vec<Tool>>,
) -> Option<SystemMessage> {
    let has_prompt = system_prompt
        .as_ref()
        .is_some_and(|prompt| !prompt.is_empty());
    let has_tools = tools.as_ref().is_some_and(|tools| !tools.is_empty());
    if !has_prompt && !has_tools {
        return None;
    }
    let mut message = SystemMessage::new(system_prompt.unwrap_or_default(), 0);
    if has_tools {
        message.tools_added = tools;
    }
    Some(message)
}

/// Fold `Context` shorthand into a leading system message.
#[must_use]
pub fn normalize_context(context: Context) -> TranscriptContext {
    let initial = create_initial_system_message(context.system_prompt, context.tools);
    let mut messages = context.messages;
    if let Some(initial) = initial {
        let mut normalized = Vec::with_capacity(messages.len().saturating_add(1));
        normalized.push(Message::System(Box::new(initial)));
        normalized.append(&mut messages);
        return TranscriptContext {
            messages: normalized,
        };
    }
    TranscriptContext { messages }
}

/// Return the leading system message when the transcript starts with one.
#[must_use]
pub fn get_initial_system_message(messages: &[Message]) -> Option<&SystemMessage> {
    match messages.first() {
        Some(Message::System(message)) => Some(message),
        _ => None,
    }
}

fn apply_tool_deltas(state: &mut IndexMap<String, Tool>, message: &SystemMessage) {
    if let Some(removed) = message.tools_removed.as_ref() {
        for tool in removed {
            state.shift_remove(tool.name.as_str());
        }
    }
    if let Some(added) = message.tools_added.as_ref() {
        for tool in added {
            state.insert(tool.name.clone(), tool.clone());
        }
    }
}

/// Resolve the tools available after applying every transcript delta in order.
#[must_use]
pub fn get_current_tools(messages: &[Message]) -> Vec<Tool> {
    let mut tools: IndexMap<String, Tool> = IndexMap::new();
    for message in messages {
        if let Message::System(system) = message {
            apply_tool_deltas(&mut tools, system);
        }
    }
    tools.into_values().collect()
}

/// Replay every system message into one leading message with current prompt and tools.
#[must_use]
pub fn get_current_system_message(messages: &[Message]) -> Option<SystemMessage> {
    let mut content_parts = Vec::new();
    let mut sections: IndexMap<String, String> = IndexMap::new();
    let mut timestamp: Option<i64> = None;
    for message in messages {
        let Message::System(system) = message else {
            continue;
        };
        if timestamp.is_none() {
            timestamp = Some(system.timestamp);
        }
        let text = system_content_text(&system.content);
        if !text.is_empty() {
            content_parts.push(text);
        }
        if let Some(update) = system.sections.as_ref() {
            for (name, value) in update {
                match value {
                    None => {
                        sections.shift_remove(name.as_str());
                    }
                    Some(value) => {
                        sections.insert(name.clone(), value.clone());
                    }
                }
            }
        }
    }
    let tools = get_current_tools(messages);
    if timestamp.is_none() && tools.is_empty() {
        return None;
    }
    let mut current = SystemMessage::new(content_parts.join("\n\n"), timestamp.unwrap_or(0));
    if !sections.is_empty() {
        current.sections = Some(
            sections
                .into_iter()
                .map(|(name, value)| (name, Some(value)))
                .collect(),
        );
    }
    if !tools.is_empty() {
        current.tools_added = Some(tools);
    }
    Some(current)
}

/// Render the current system prompt text after replaying every system message.
#[must_use]
pub fn get_current_system_prompt(messages: &[Message]) -> String {
    get_current_system_message(messages)
        .as_ref()
        .map(get_system_message_text)
        .unwrap_or_default()
}

/// Rebuild the transcript for APIs without mid-conversation system messages.
#[must_use]
pub fn collapse_system_messages(context: TranscriptContext) -> TranscriptContext {
    let head = get_current_system_message(&context.messages);
    let mut messages = Vec::with_capacity(context.messages.len().saturating_add(1));
    for message in context.messages {
        if !matches!(message, Message::System(_)) {
            messages.push(message);
        }
    }
    match head {
        Some(head) => {
            let mut collapsed = Vec::with_capacity(messages.len().saturating_add(1));
            collapsed.push(Message::System(Box::new(head)));
            collapsed.extend(messages);
            TranscriptContext {
                messages: collapsed,
            }
        }
        None => TranscriptContext { messages },
    }
}

/// Keep later system messages when the model accepts them, else collapse them.
#[must_use]
pub fn resolve_transcript(
    context: TranscriptContext,
    supports_mid_convo_system_messages: bool,
) -> TranscriptContext {
    if supports_mid_convo_system_messages {
        context
    } else {
        collapse_system_messages(context)
    }
}

fn declarations_equal(left: &Tool, right: &Tool) -> bool {
    left == right
}

/// Compare two complete tool states.
#[must_use]
pub fn get_tool_state_changes(previous: &[Tool], current: &[Tool]) -> ToolStateChanges {
    let mut previous_by_name: IndexMap<&str, &Tool> = IndexMap::new();
    for tool in previous {
        previous_by_name.insert(tool.name.as_str(), tool);
    }
    let mut current_by_name: IndexMap<&str, &Tool> = IndexMap::new();
    for tool in current {
        current_by_name.insert(tool.name.as_str(), tool);
    }
    let mut tools_added = Vec::new();
    for tool in current {
        match previous_by_name.get(tool.name.as_str()) {
            Some(previous) if declarations_equal(previous, tool) => {}
            _ => tools_added.push(tool.clone()),
        }
    }
    let mut tools_removed = Vec::new();
    for tool in previous {
        match current_by_name.get(tool.name.as_str()) {
            Some(current) if declarations_equal(tool, current) => {}
            _ => tools_removed.push(ToolReference {
                name: tool.name.clone(),
            }),
        }
    }
    ToolStateChanges {
        tools_added,
        tools_removed,
    }
}

/// Every definition referenced by transcript tool state, in first-declaration order.
#[must_use]
pub fn get_declared_tools(messages: &[Message]) -> Vec<Tool> {
    let mut definitions: IndexMap<String, Tool> = IndexMap::new();
    for message in messages {
        if let Message::System(system) = message
            && let Some(added) = system.tools_added.as_ref()
        {
            for tool in added {
                definitions.insert(tool.name.clone(), tool.clone());
            }
        }
    }
    definitions.into_values().collect()
}

/// Whether a tool name was declared twice with different definitions.
#[must_use]
pub fn has_tool_redefinitions(messages: &[Message]) -> bool {
    let mut declared: IndexMap<String, Tool> = IndexMap::new();
    for message in messages {
        if let Message::System(system) = message
            && let Some(added) = system.tools_added.as_ref()
        {
            for tool in added {
                match declared.get(tool.name.as_str()) {
                    Some(previous) if !declarations_equal(previous, tool) => return true,
                    _ => {}
                }
                declared.insert(tool.name.clone(), tool.clone());
            }
        }
    }
    false
}

/// Whether history contains a removal or redeclaration that addition-only transports miss.
#[must_use]
pub fn has_non_additive_tool_changes(messages: &[Message]) -> bool {
    let mut declared = std::collections::BTreeSet::new();
    for message in messages {
        if let Message::System(system) = message {
            if system
                .tools_removed
                .as_ref()
                .is_some_and(|removed| !removed.is_empty())
            {
                return true;
            }
            if let Some(added) = system.tools_added.as_ref() {
                for tool in added {
                    if !declared.insert(tool.name.as_str()) {
                        return true;
                    }
                }
            }
        }
    }
    false
}

/// Split tool declarations between top-level request field and in-place additions.
#[must_use]
pub fn resolve_transcript_tools(
    messages: &[Message],
    supports_tool_additions: bool,
) -> TranscriptTools {
    let anchors_additions = supports_tool_additions && !has_non_additive_tool_changes(messages);
    let request_tools = if anchors_additions {
        get_initial_system_message(messages)
            .and_then(|initial| initial.tools_added.clone())
            .unwrap_or_default()
    } else {
        get_current_tools(messages)
    };
    TranscriptTools {
        request_tools,
        anchors_additions,
    }
}
