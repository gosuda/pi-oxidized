//! Session-entry to context-message projection for compaction.
//!
//! Ported from `.references/pi/packages/agent/src/harness/session/context.ts`.
//! Only the pieces the compaction algorithms consume live here; the runtime
//! owns full session context building.

use pi_ai::{Message, StopReason};

use crate::message::AgentMessage;
use crate::session::Entry;

use super::messages::{create_branch_summary_message, create_compaction_summary_message};

/// Whether a message participates in LLM context.
///
/// Assistant messages that terminated in `Error`, `Aborted`, or `Deferred`
/// are excluded; every other message is kept.
#[must_use]
pub fn is_context_message(message: &AgentMessage) -> bool {
    match message {
        AgentMessage::Llm(message) => match message.as_ref() {
            Message::Assistant(assistant) => !matches!(
                assistant.stop_reason,
                StopReason::Error | StopReason::Aborted | StopReason::Deferred
            ),
            _ => true,
        },
        AgentMessage::Custom(_) => true,
    }
}

/// Returns the latest compaction entry and the entries after it. If the path
/// has no compaction, all path entries are retained.
///
/// Mirrors `buildContextEntries` in `context.ts`. Custom entries remain in
/// this structural path so a caller with an entry projector can handle them;
/// `session_entry_to_context_messages` itself emits no message for them.
#[must_use]
pub fn build_context_entries(path_entries: &[Entry]) -> Vec<&Entry> {
    let latest_compaction = path_entries
        .iter()
        .rposition(|entry| matches!(entry, Entry::Compaction { .. }));
    match latest_compaction {
        Some(index) => path_entries[index..].iter().collect(),
        None => path_entries.iter().collect(),
    }
}

/// Projects one entry into the messages it contributes to LLM context.
///
/// Mirrors `sessionEntryToContextMessages` in `context.ts`: compaction
/// entries emit their summary message plus the context-relevant retained
/// tail; branch summaries emit their summary message unless empty.
#[must_use]
pub fn session_entry_to_context_messages(entry: &Entry) -> Vec<AgentMessage> {
    match entry {
        Entry::Message { message, .. } => {
            if is_context_message(message) {
                vec![message.clone()]
            } else {
                Vec::new()
            }
        }
        Entry::Compaction {
            base,
            summary,
            retained_tail,
            tokens_before,
            ..
        } => {
            let mut messages = vec![create_compaction_summary_message(
                summary,
                *tokens_before,
                base.timestamp,
            )];
            messages.extend(
                retained_tail
                    .iter()
                    .filter(|message| is_context_message(message))
                    .cloned(),
            );
            messages
        }
        Entry::BranchSummary {
            base,
            from_id,
            summary,
            ..
        } => {
            if summary.is_empty() {
                Vec::new()
            } else {
                vec![create_branch_summary_message(
                    summary,
                    from_id.as_ref(),
                    base.timestamp,
                )]
            }
        }
        Entry::Custom { .. } => Vec::new(),
    }
}

#[expect(clippy::panic, reason = "test assertions use let-else panic for irrecoverable fixture mismatch")]
#[cfg(test)]
mod tests {
    use super::*;
    use pi_ai::{AssistantContent, AssistantMessage, TextContent, Usage};

    use crate::session::{EntryBase, EntryId};

    fn base(id: &str) -> EntryBase {
        EntryBase {
            id: EntryId::new(id),
            parent_id: None,
            seq: 0,
            timestamp: 1,
            custom_type: None,
        }
    }

    fn assistant(stop_reason: StopReason) -> AgentMessage {
        let mut message = AssistantMessage::new("k", "m", "p", 1);
        message.content = vec![AssistantContent::Text(TextContent::new("hi"))];
        message.stop_reason = stop_reason;
        message.usage = Usage::default();
        AgentMessage::Llm(Box::new(Message::Assistant(Box::new(message))))
    }

    #[test]
    fn context_message_filters_failed_assistants() {
        assert!(is_context_message(&assistant(StopReason::Stop)));
        assert!(!is_context_message(&assistant(StopReason::Error)));
        assert!(!is_context_message(&assistant(StopReason::Aborted)));
        assert!(!is_context_message(&assistant(StopReason::Deferred)));
    }

    #[test]
    fn build_context_entries_keeps_latest_compaction_tail() {
        let entries = vec![
            Entry::Message {
                base: base("a"),
                message: assistant(StopReason::Stop),
                terminate: false,
            },
            Entry::Compaction {
                base: base("c"),
                summary: "sum".to_owned(),
                retained_tail: Vec::new(),
                tokens_before: 10,
                details: None,
                usage: None,
                from_hook: false,
            },
            Entry::Message {
                base: base("b"),
                message: assistant(StopReason::Stop),
                terminate: false,
            },
            Entry::Custom {
                base: base("d"),
                custom_type: "opaque".to_owned(),
                data: None,
            },
        ];
        let kept = build_context_entries(&entries);
        assert_eq!(kept.len(), 3);
        assert_eq!(kept[0].id().as_str(), "c");
        assert_eq!(kept[2].id().as_str(), "d");
    }

    #[test]
    fn compaction_entry_projects_summary_and_tail() {
        let entry = Entry::Compaction {
            base: base("c"),
            summary: "sum".to_owned(),
            retained_tail: vec![assistant(StopReason::Stop), assistant(StopReason::Error)],
            tokens_before: 10,
            details: None,
            usage: None,
            from_hook: false,
        };
        let messages = session_entry_to_context_messages(&entry);
        assert_eq!(messages.len(), 2);
        let AgentMessage::Custom(summary) = &messages[0] else {
            panic!("expected summary message");
        };
        assert_eq!(summary.role, "compactionSummary");
    }

    #[test]
    fn empty_branch_summary_projects_nothing() {
        let entry = Entry::BranchSummary {
            base: base("b"),
            from_id: None,
            summary: String::new(),
            details: None,
            usage: None,
            from_hook: false,
        };
        assert!(session_entry_to_context_messages(&entry).is_empty());
    }
}
