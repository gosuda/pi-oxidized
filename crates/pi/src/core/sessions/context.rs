//! Session path, compaction-aware context, and entry→message projection.
//!
//! Ports `buildSessionPath`, `buildContextEntries`, `buildSessionContext`, and
//! `sessionEntryToContextMessages` from
//! `.references/pi/packages/coding-agent/src/core/session-manager.ts`.

use std::collections::HashMap;

use pi_agent::{AgentMessage, CustomAgentMessage};
use pi_ai::Message;
use serde::de::Error as _;
use serde_json::Value;

use super::super::messages::{
    MessageConversionError, create_branch_summary_message, create_compaction_summary_message,
    create_custom_message,
};
use super::entries::{
    CompactionEntry, ModelChangeEntry, SessionEntry, SessionMessageEntry, ThinkingLevelChangeEntry,
};

/// Default thinking level when no `thinking_level_change` is on the path.
pub const DEFAULT_THINKING_LEVEL: &str = "off";

/// Leaf-selection semantics for path/context construction.
///
/// Mirrors the TypeScript `leafId?: string | null` argument:
/// - [`LeafRef::Last`] — argument omitted (`undefined`): start from the last entry.
/// - [`LeafRef::Null`] — explicit `null`: empty path.
/// - [`LeafRef::Id`] — start from that id (missing id falls back to last entry).
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum LeafRef<'a> {
    /// Use the last entry in file order (TS `undefined`).
    Last,
    /// Empty path (TS `null`).
    Null,
    /// Walk from this entry id.
    Id(&'a str),
}

/// Provider/model pair resolved from the active path.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SessionModel {
    /// Provider id.
    pub provider: String,
    /// Model id within the provider.
    pub model_id: String,
}

/// Resolved session context for the LLM turn.
#[derive(Clone, Debug, PartialEq)]
pub struct SessionContext {
    /// Messages projected from the compaction-aware path.
    pub messages: Vec<AgentMessage>,
    /// Last thinking level on the full path (default `"off"`).
    pub thinking_level: String,
    /// Last model from a `model_change` or assistant message on the full path.
    pub model: Option<SessionModel>,
}

/// Walk from leaf to root, returning entries in root→leaf order.
///
/// When `leaf` is [`LeafRef::Id`] and the id is not in the index, falls back to
/// the last entry (TypeScript `buildSessionPath` behavior).
#[must_use]
pub fn build_session_path<'a>(
    entries: &[&'a SessionEntry],
    leaf: LeafRef<'a>,
) -> Vec<&'a SessionEntry> {
    let index = build_entry_index(entries);

    let start: Option<&'a SessionEntry> = match leaf {
        LeafRef::Null => return Vec::new(),
        LeafRef::Last => entries.last().copied(),
        LeafRef::Id(id) => {
            if id.is_empty() {
                entries.last().copied()
            } else {
                index.get(id).copied().or_else(|| entries.last().copied())
            }
        }
    };

    let Some(mut current) = start else {
        return Vec::new();
    };

    let mut path = Vec::new();
    loop {
        path.push(current);
        match current.parent_id() {
            Some(pid) => match index.get(pid) {
                Some(next) => current = next,
                None => break,
            },
            None => break,
        }
    }
    path.reverse();
    path
}

/// Build the compaction-aware active entry list for context/rendering.
///
/// Order: `[latest compaction on path]` + path entries from `firstKeptEntryId`
/// up to (but not including) the compaction + all entries after the compaction.
/// Older summarized entries are omitted. Settings must still be read from the
/// full path via [`build_session_context`].
#[must_use]
pub fn build_context_entries<'a>(
    entries: &[&'a SessionEntry],
    leaf: LeafRef<'a>,
) -> Vec<&'a SessionEntry> {
    let path = build_session_path(entries, leaf);

    let mut compaction: Option<&'a SessionEntry> = None;
    for entry in &path {
        if entry.discriminant() == "compaction" {
            compaction = Some(*entry);
        }
    }

    let Some(compaction) = compaction else {
        return path;
    };

    let compaction_id = compaction.id();
    let compaction_idx = path.iter().position(|e| e.id() == compaction_id);
    let Some(compaction_idx) = compaction_idx else {
        return path;
    };

    let first_kept = match compaction {
        SessionEntry::Compaction(c) => Some(c.first_kept_entry_id.as_str()),
        SessionEntry::Unknown(raw) => raw.get("firstKeptEntryId").and_then(Value::as_str),
        _ => None,
    };

    let mut context: Vec<&'a SessionEntry> = vec![compaction];
    let mut found_first_kept = false;
    for entry in path.iter().take(compaction_idx) {
        if first_kept.is_some() && entry.id() == first_kept {
            found_first_kept = true;
        }
        if found_first_kept {
            context.push(*entry);
        }
    }
    context.extend(path.iter().skip(compaction_idx + 1).copied());
    context
}

/// Build the session context (messages + thinking level + model) for the LLM.
///
/// # Errors
///
/// Returns a [`MessageConversionError`] if any entry on the active path fails
/// to project into agent messages (e.g. a malformed custom/compaction payload).
pub fn build_session_context(
    entries: &[&SessionEntry],
    leaf: LeafRef<'_>,
) -> Result<SessionContext, MessageConversionError> {
    let path = build_session_path(entries, leaf);
    let (thinking_level, model) = get_session_context_settings(&path);
    let context_entries = build_context_entries(entries, leaf);
    let mut messages = Vec::new();
    for entry in context_entries {
        messages.extend(session_entry_to_context_messages(entry)?);
    }
    Ok(SessionContext {
        messages,
        thinking_level,
        model,
    })
}

/// Project one selected session entry into runtime/LLM messages.
///
/// Plain custom / label / model / thinking entries return an empty list.
///
/// # Errors
///
/// Returns a [`MessageConversionError`] if the entry is a custom, branch
/// summary, or compaction entry whose payload cannot be serialized or wrapped
/// into an agent message.
pub fn session_entry_to_context_messages(
    entry: &SessionEntry,
) -> Result<Vec<AgentMessage>, MessageConversionError> {
    match entry {
        SessionEntry::Message(m) => Ok(vec![m.message.clone()]),
        SessionEntry::CustomMessage(c) => {
            let custom = create_custom_message(
                &c.custom_type,
                c.content.clone(),
                c.display,
                c.details.clone(),
                &c.timestamp,
            )?;
            Ok(vec![product_to_agent_message(&custom)?])
        }
        SessionEntry::BranchSummary(b) if !b.summary.is_empty() => {
            let msg = create_branch_summary_message(&b.summary, &b.from_id, &b.timestamp)?;
            Ok(vec![product_to_agent_message(&msg)?])
        }
        SessionEntry::Compaction(c) => {
            let msg = create_compaction_summary_message(&c.summary, c.tokens_before, &c.timestamp)?;
            Ok(vec![product_to_agent_message(&msg)?])
        }
        _ => Ok(Vec::new()),
    }
}

/// Latest compaction entry on a list (reverse scan), if any.
#[must_use]
pub fn get_latest_compaction_entry<'a>(
    entries: &[&'a SessionEntry],
) -> Option<&'a CompactionEntry> {
    for entry in entries.iter().rev() {
        if let SessionEntry::Compaction(c) = entry {
            return Some(c);
        }
    }
    None
}

// ---------------------------------------------------------------------------
// Internals
// ---------------------------------------------------------------------------

fn build_entry_index<'a>(entries: &[&'a SessionEntry]) -> HashMap<&'a str, &'a SessionEntry> {
    let mut index = HashMap::with_capacity(entries.len());
    for entry in entries {
        if let Some(id) = entry.id() {
            index.insert(id, *entry);
        }
    }
    index
}

fn get_session_context_settings(path: &[&SessionEntry]) -> (String, Option<SessionModel>) {
    let mut thinking_level = DEFAULT_THINKING_LEVEL.to_owned();
    let mut model: Option<SessionModel> = None;

    for entry in path {
        match entry {
            SessionEntry::ThinkingLevelChange(ThinkingLevelChangeEntry {
                thinking_level: level,
                ..
            }) => {
                thinking_level.clone_from(level);
            }
            SessionEntry::ModelChange(ModelChangeEntry {
                provider, model_id, ..
            }) => {
                model = Some(SessionModel {
                    provider: provider.clone(),
                    model_id: model_id.clone(),
                });
            }
            SessionEntry::Message(SessionMessageEntry { message, .. })
                if message.role() == "assistant" =>
            {
                if let Some(Message::Assistant(a)) = message.as_llm() {
                    model = Some(SessionModel {
                        provider: a.provider.clone(),
                        model_id: a.model.clone(),
                    });
                }
            }
            _ => {}
        }
    }

    (thinking_level, model)
}

fn product_to_agent_message<T: serde::Serialize>(
    msg: &T,
) -> Result<AgentMessage, MessageConversionError> {
    let value =
        serde_json::to_value(msg).map_err(|source| MessageConversionError::InvalidPayload {
            role: "product",
            source,
        })?;
    let Value::Object(mut map) = value else {
        return Err(MessageConversionError::InvalidPayload {
            role: "product",
            source: serde_json::Error::custom("product message must serialize to an object"),
        });
    };
    let Some(Value::String(role)) = map.remove("role") else {
        return Err(MessageConversionError::InvalidPayload {
            role: "product",
            source: serde_json::Error::custom("product message missing string role"),
        });
    };
    Ok(AgentMessage::Custom(CustomAgentMessage::new(role, map)))
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    type TestResult = Result<(), Box<dyn std::error::Error>>;
    type FixtureResult = Result<SessionEntry, Box<dyn std::error::Error>>;

    fn msg(id: &str, parent: Option<&str>, role: &str, text: &str) -> FixtureResult {
        let value = if role == "user" {
            json!({
                "type": "message",
                "id": id,
                "parentId": parent,
                "timestamp": "2025-01-01T00:00:00Z",
                "message": { "role": "user", "content": text, "timestamp": 1 }
            })
        } else {
            json!({
                "type": "message",
                "id": id,
                "parentId": parent,
                "timestamp": "2025-01-01T00:00:00Z",
                "message": {
                    "role": "assistant",
                    "content": [{ "type": "text", "text": text }],
                    "api": "anthropic-messages",
                    "provider": "anthropic",
                    "model": "claude-test",
                    "usage": {
                        "input": 1, "output": 1, "cacheRead": 0, "cacheWrite": 0,
                        "totalTokens": 2,
                        "cost": { "input": 0, "output": 0, "cacheRead": 0, "cacheWrite": 0, "total": 0 }
                    },
                    "stopReason": "stop",
                    "timestamp": 1
                }
            })
        };
        Ok(serde_json::from_value(value)?)
    }

    fn compaction(
        id: &str,
        parent: Option<&str>,
        summary: &str,
        first_kept: &str,
    ) -> FixtureResult {
        Ok(serde_json::from_value(json!({
            "type": "compaction",
            "id": id,
            "parentId": parent,
            "timestamp": "2025-01-01T00:00:00Z",
            "summary": summary,
            "firstKeptEntryId": first_kept,
            "tokensBefore": 1000
        }))?)
    }

    fn branch_summary(
        id: &str,
        parent: Option<&str>,
        summary: &str,
        from_id: &str,
    ) -> FixtureResult {
        Ok(serde_json::from_value(json!({
            "type": "branch_summary",
            "id": id,
            "parentId": parent,
            "timestamp": "2025-01-01T00:00:00Z",
            "summary": summary,
            "fromId": from_id
        }))?)
    }

    fn custom(id: &str, parent: Option<&str>, custom_type: &str) -> FixtureResult {
        Ok(serde_json::from_value(json!({
            "type": "custom",
            "id": id,
            "parentId": parent,
            "timestamp": "2025-01-01T00:00:00Z",
            "customType": custom_type,
            "data": { "x": 1 }
        }))?)
    }

    fn thinking(id: &str, parent: Option<&str>, level: &str) -> FixtureResult {
        Ok(serde_json::from_value(json!({
            "type": "thinking_level_change",
            "id": id,
            "parentId": parent,
            "timestamp": "2025-01-01T00:00:00Z",
            "thinkingLevel": level
        }))?)
    }

    fn model_change(
        id: &str,
        parent: Option<&str>,
        provider: &str,
        model_id: &str,
    ) -> FixtureResult {
        Ok(serde_json::from_value(json!({
            "type": "model_change",
            "id": id,
            "parentId": parent,
            "timestamp": "2025-01-01T00:00:00Z",
            "provider": provider,
            "modelId": model_id
        }))?)
    }

    fn refs(entries: &[SessionEntry]) -> Vec<&SessionEntry> {
        entries.iter().collect()
    }

    #[test]
    fn empty_entries_empty_context() -> TestResult {
        let ctx = build_session_context(&[], LeafRef::Last)?;
        assert!(ctx.messages.is_empty());
        assert_eq!(ctx.thinking_level, "off");
        assert!(ctx.model.is_none());
        Ok(())
    }

    #[test]
    fn simple_conversation() -> TestResult {
        let entries = [
            msg("1", None, "user", "hello")?,
            msg("2", Some("1"), "assistant", "hi there")?,
            msg("3", Some("2"), "user", "how are you")?,
            msg("4", Some("3"), "assistant", "great")?,
        ];
        let r = refs(&entries);
        let ctx = build_session_context(&r, LeafRef::Last)?;
        assert_eq!(ctx.messages.len(), 4);
        assert_eq!(
            ctx.messages
                .iter()
                .map(AgentMessage::role)
                .collect::<Vec<_>>(),
            vec!["user", "assistant", "user", "assistant"]
        );
        Ok(())
    }

    #[test]
    fn tracks_thinking_level() -> TestResult {
        let entries = [
            msg("1", None, "user", "hello")?,
            thinking("2", Some("1"), "high")?,
            msg("3", Some("2"), "assistant", "thinking hard")?,
        ];
        let r = refs(&entries);
        let ctx = build_session_context(&r, LeafRef::Last)?;
        assert_eq!(ctx.thinking_level, "high");
        assert_eq!(ctx.messages.len(), 2);
        Ok(())
    }

    #[test]
    fn tracks_model_from_assistant() -> TestResult {
        let entries = [
            msg("1", None, "user", "hello")?,
            msg("2", Some("1"), "assistant", "hi")?,
        ];
        let r = refs(&entries);
        let ctx = build_session_context(&r, LeafRef::Last)?;
        let model = ctx.model.ok_or("model")?;
        assert_eq!(model.provider, "anthropic");
        assert_eq!(model.model_id, "claude-test");
        Ok(())
    }

    #[test]
    fn assistant_overwrites_model_change() -> TestResult {
        let entries = [
            msg("1", None, "user", "hello")?,
            model_change("2", Some("1"), "openai", "gpt-4")?,
            msg("3", Some("2"), "assistant", "hi")?,
        ];
        let r = refs(&entries);
        let ctx = build_session_context(&r, LeafRef::Last)?;
        let model = ctx.model.ok_or("model")?;
        assert_eq!(model.provider, "anthropic");
        assert_eq!(model.model_id, "claude-test");
        Ok(())
    }

    #[test]
    fn compaction_includes_summary_before_kept() -> TestResult {
        let entries = [
            msg("1", None, "user", "first")?,
            msg("2", Some("1"), "assistant", "response1")?,
            msg("3", Some("2"), "user", "second")?,
            msg("4", Some("3"), "assistant", "response2")?,
            compaction("5", Some("4"), "Summary of first two turns", "3")?,
            msg("6", Some("5"), "user", "third")?,
            msg("7", Some("6"), "assistant", "response3")?,
        ];
        let r = refs(&entries);
        let ctx = build_session_context(&r, LeafRef::Last)?;
        assert_eq!(ctx.messages.len(), 5);
        assert_eq!(ctx.messages[0].role(), "compactionSummary");
        let AgentMessage::Custom(c) = &ctx.messages[0] else {
            return Err("expected custom".into());
        };
        let summary = c
            .payload
            .get("summary")
            .and_then(Value::as_str)
            .ok_or("summary")?;
        assert!(summary.contains("Summary of first two turns"));
        Ok(())
    }

    #[test]
    fn multiple_compactions_uses_latest() -> TestResult {
        let entries = [
            msg("1", None, "user", "a")?,
            msg("2", Some("1"), "assistant", "b")?,
            compaction("3", Some("2"), "First summary", "1")?,
            msg("4", Some("3"), "user", "c")?,
            msg("5", Some("4"), "assistant", "d")?,
            compaction("6", Some("5"), "Second summary", "4")?,
            msg("7", Some("6"), "user", "e")?,
        ];
        let r = refs(&entries);
        let ctx = build_session_context(&r, LeafRef::Last)?;
        assert_eq!(ctx.messages.len(), 4);
        let AgentMessage::Custom(c) = &ctx.messages[0] else {
            return Err("expected custom".into());
        };
        let summary = c
            .payload
            .get("summary")
            .and_then(Value::as_str)
            .ok_or("summary")?;
        assert!(summary.contains("Second summary"));
        Ok(())
    }

    #[test]
    fn build_context_entries_includes_custom_on_path() -> TestResult {
        let entries = [
            msg("1", None, "user", "first")?,
            custom("2", Some("1"), "old-state")?,
            msg("3", Some("2"), "assistant", "response1")?,
            custom("4", Some("3"), "kept-card")?,
            msg("5", Some("4"), "user", "second")?,
            compaction("6", Some("5"), "Summary", "4")?,
            custom("7", Some("6"), "after-card")?,
            msg("8", Some("7"), "assistant", "response2")?,
        ];
        let r = refs(&entries);
        let ids: Vec<&str> = build_context_entries(&r, LeafRef::Last)
            .iter()
            .filter_map(|e| e.id())
            .collect();
        assert_eq!(ids, vec!["6", "4", "5", "7", "8"]);
        Ok(())
    }

    #[test]
    fn settings_from_full_path_after_compaction() -> TestResult {
        let entries = [
            msg("1", None, "user", "first")?,
            thinking("2", Some("1"), "high")?,
            msg("3", Some("2"), "assistant", "response1")?,
            msg("4", Some("3"), "user", "second")?,
            compaction("5", Some("4"), "Summary", "4")?,
        ];
        let r = refs(&entries);
        let ctx = build_session_context(&r, LeafRef::Last)?;
        assert_eq!(ctx.thinking_level, "high");
        assert_eq!(
            ctx.messages
                .iter()
                .map(AgentMessage::role)
                .collect::<Vec<_>>(),
            vec!["compactionSummary", "user"]
        );
        Ok(())
    }

    #[test]
    fn follows_path_to_specified_leaf() -> TestResult {
        let entries = [
            msg("1", None, "user", "start")?,
            msg("2", Some("1"), "assistant", "response")?,
            msg("3", Some("2"), "user", "branch A")?,
            msg("4", Some("2"), "user", "branch B")?,
        ];
        let r = refs(&entries);
        let ctx_a = build_session_context(&r, LeafRef::Id("3"))?;
        assert_eq!(ctx_a.messages.len(), 3);
        let ctx_b = build_session_context(&r, LeafRef::Id("4"))?;
        assert_eq!(ctx_b.messages.len(), 3);
        Ok(())
    }

    #[test]
    fn includes_branch_summary_in_path() -> TestResult {
        let entries = [
            msg("1", None, "user", "start")?,
            msg("2", Some("1"), "assistant", "response")?,
            msg("3", Some("2"), "user", "abandoned path")?,
            branch_summary("4", Some("2"), "Summary of abandoned work", "3")?,
            msg("5", Some("4"), "user", "new direction")?,
        ];
        let r = refs(&entries);
        let ctx = build_session_context(&r, LeafRef::Id("5"))?;
        assert_eq!(ctx.messages.len(), 4);
        assert_eq!(ctx.messages[2].role(), "branchSummary");
        Ok(())
    }

    #[test]
    fn missing_leaf_id_falls_back_to_last() -> TestResult {
        let entries = [
            msg("1", None, "user", "hello")?,
            msg("2", Some("1"), "assistant", "hi")?,
        ];
        let r = refs(&entries);
        let ctx = build_session_context(&r, LeafRef::Id("nonexistent"))?;
        assert_eq!(ctx.messages.len(), 2);
        Ok(())
    }

    #[test]
    fn orphaned_entry_path() -> TestResult {
        let entries = [
            msg("1", None, "user", "hello")?,
            msg("2", Some("missing"), "assistant", "orphan")?,
        ];
        let r = refs(&entries);
        let ctx = build_session_context(&r, LeafRef::Id("2"))?;
        assert_eq!(ctx.messages.len(), 1);
        Ok(())
    }

    #[test]
    fn custom_entries_not_in_messages() -> TestResult {
        let entries = [
            msg("1", None, "user", "hello")?,
            custom("2", Some("1"), "my_data")?,
            msg("3", Some("2"), "assistant", "hi")?,
        ];
        let r = refs(&entries);
        let ctx = build_session_context(&r, LeafRef::Last)?;
        assert_eq!(ctx.messages.len(), 2);
        Ok(())
    }
}
