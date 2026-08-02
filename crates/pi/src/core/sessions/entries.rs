//! Session JSONL entry types, load/parse helpers, and v1→v2→v3 migrations.
//!
//! Ports the entry catalog and in-file migrations from
//! `.references/pi/packages/coding-agent/src/core/session-manager.ts`.
//! Unknown entry variants keep their raw JSON so rewrites and forks never drop
//! future fields; typed variants preserve unrecognized sibling keys via
//! `#[serde(flatten)]`.

use std::collections::HashSet;
use std::fs::File;
use std::io::{self, BufRead, BufReader, Read};
use std::path::Path;
use std::time::{SystemTime, UNIX_EPOCH};

use pi_agent::AgentMessage;
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};
use uuid::Uuid;

use super::super::config::{PathInputOptions, normalize_path};
use super::super::messages::CustomMessageContent;

/// Current on-disk session format version written by new sessions.
pub const CURRENT_SESSION_VERSION: u64 = 3;

/// First-line header read window used by [`read_session_header`].
const SESSION_HEADER_PROBE_BYTES: usize = 512;

/// Placeholder first-message text when a session has no user text.
pub const NO_MESSAGES_PLACEHOLDER: &str = "(no messages)";

// ---------------------------------------------------------------------------
// Tags (private wire discriminants)
// ---------------------------------------------------------------------------

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
enum SessionTag {
    #[serde(rename = "session")]
    Session,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
enum MessageTag {
    #[serde(rename = "message")]
    Message,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
enum ThinkingLevelChangeTag {
    #[serde(rename = "thinking_level_change")]
    ThinkingLevelChange,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
enum ModelChangeTag {
    #[serde(rename = "model_change")]
    ModelChange,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
enum CompactionTag {
    #[serde(rename = "compaction")]
    Compaction,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
enum BranchSummaryTag {
    #[serde(rename = "branch_summary")]
    BranchSummary,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
enum CustomTag {
    #[serde(rename = "custom")]
    Custom,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
enum CustomMessageTag {
    #[serde(rename = "custom_message")]
    CustomMessage,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
enum LabelTag {
    #[serde(rename = "label")]
    Label,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
enum SessionInfoTag {
    #[serde(rename = "session_info")]
    SessionInfo,
}

// ---------------------------------------------------------------------------
// Header
// ---------------------------------------------------------------------------

/// Session file header (`type: "session"`). First parseable line of a valid file.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SessionHeader {
    #[serde(rename = "type")]
    kind: SessionTag,
    /// Format version. Absent on v1 files; written as 3 for new sessions.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub version: Option<u64>,
    /// Session UUID (uuidv7 on create). Validated as a string by the loader.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub id: Option<String>,
    /// ISO-8601 timestamp with millisecond precision and `Z` suffix.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub timestamp: Option<String>,
    /// Working directory captured at session creation (resolved absolute path).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cwd: Option<String>,
    /// Filesystem path of the parent session when this session was forked/branched.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parent_session: Option<String>,
    /// Unknown header fields preserved across rewrites.
    #[serde(flatten)]
    pub extra: Map<String, Value>,
}

impl SessionHeader {
    /// Build a v3 header for a new session.
    #[must_use]
    pub fn new(
        id: impl Into<String>,
        timestamp: impl Into<String>,
        cwd: impl Into<String>,
        parent_session: Option<String>,
    ) -> Self {
        Self {
            kind: SessionTag::Session,
            version: Some(CURRENT_SESSION_VERSION),
            id: Some(id.into()),
            timestamp: Some(timestamp.into()),
            cwd: Some(cwd.into()),
            parent_session,
            extra: Map::new(),
        }
    }
}

// ---------------------------------------------------------------------------
// Typed entry structs
// ---------------------------------------------------------------------------

/// Transcript message entry.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SessionMessageEntry {
    #[serde(rename = "type")]
    kind: MessageTag,
    /// Short entry id (8-hex v4 prefix, collision-checked).
    pub id: String,
    /// Parent entry id; `None` serializes as JSON `null` (root).
    pub parent_id: Option<String>,
    /// ISO-8601 entry timestamp.
    pub timestamp: String,
    /// Agent transcript message (user/assistant/toolResult or product custom).
    pub message: AgentMessage,
    /// Unknown sibling fields preserved across rewrites.
    #[serde(flatten)]
    pub extra: Map<String, Value>,
}

/// Thinking-level change entry (settings only; not in LLM context).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ThinkingLevelChangeEntry {
    #[serde(rename = "type")]
    kind: ThinkingLevelChangeTag,
    /// Short entry id.
    pub id: String,
    /// Parent entry id.
    pub parent_id: Option<String>,
    /// ISO-8601 entry timestamp.
    pub timestamp: String,
    /// Thinking level string (for example `"off"`, `"high"`).
    pub thinking_level: String,
    /// Unknown sibling fields.
    #[serde(flatten)]
    pub extra: Map<String, Value>,
}

/// Model-change entry (settings only; not in LLM context).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ModelChangeEntry {
    #[serde(rename = "type")]
    kind: ModelChangeTag,
    /// Short entry id.
    pub id: String,
    /// Parent entry id.
    pub parent_id: Option<String>,
    /// ISO-8601 entry timestamp.
    pub timestamp: String,
    /// Provider id.
    pub provider: String,
    /// Model id within the provider.
    pub model_id: String,
    /// Unknown sibling fields.
    #[serde(flatten)]
    pub extra: Map<String, Value>,
}

/// Compaction summary entry.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CompactionEntry {
    #[serde(rename = "type")]
    kind: CompactionTag,
    /// Short entry id.
    pub id: String,
    /// Parent entry id.
    pub parent_id: Option<String>,
    /// ISO-8601 entry timestamp.
    pub timestamp: String,
    /// Compaction summary text.
    pub summary: String,
    /// First kept entry id after compaction (context reconstruction anchor).
    pub first_kept_entry_id: String,
    /// Token count observed before compaction.
    pub tokens_before: i64,
    /// Extension-specific details (not sent to the LLM).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub details: Option<Value>,
    /// `true` when produced by an extension hook.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub from_hook: Option<bool>,
    /// LLM usage from the summarization call(s), when available.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub usage: Option<pi_ai::Usage>,
    /// Unknown sibling fields.
    #[serde(flatten)]
    pub extra: Map<String, Value>,
}

/// Branch-summary entry capturing abandoned-path context.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct BranchSummaryEntry {
    #[serde(rename = "type")]
    kind: BranchSummaryTag,
    /// Short entry id.
    pub id: String,
    /// Parent entry id.
    pub parent_id: Option<String>,
    /// ISO-8601 entry timestamp.
    pub timestamp: String,
    /// Entry id the branch forked from (`"root"` when branching from null).
    pub from_id: String,
    /// Branch summary text.
    pub summary: String,
    /// Extension-specific details (not sent to the LLM).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub details: Option<Value>,
    /// `true` when produced by an extension hook.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub from_hook: Option<bool>,
    /// LLM usage from the branch-summary call, when available.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub usage: Option<pi_ai::Usage>,
    /// Unknown sibling fields.
    #[serde(flatten)]
    pub extra: Map<String, Value>,
}

/// Extension custom state entry (not in LLM context).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CustomEntry {
    #[serde(rename = "type")]
    kind: CustomTag,
    /// Extension-defined custom type discriminant.
    pub custom_type: String,
    /// Opaque extension data.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub data: Option<Value>,
    /// Short entry id.
    pub id: String,
    /// Parent entry id.
    pub parent_id: Option<String>,
    /// ISO-8601 entry timestamp.
    pub timestamp: String,
    /// Unknown sibling fields.
    #[serde(flatten)]
    pub extra: Map<String, Value>,
}

/// Extension custom message entry (participates in LLM context).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CustomMessageEntry {
    #[serde(rename = "type")]
    kind: CustomMessageTag,
    /// Extension-defined custom type discriminant.
    pub custom_type: String,
    /// Message content (string or text/image blocks).
    pub content: CustomMessageContent,
    /// Whether the interactive UI should render this message.
    pub display: bool,
    /// Extension-specific details (not sent to the LLM).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub details: Option<Value>,
    /// Short entry id.
    pub id: String,
    /// Parent entry id.
    pub parent_id: Option<String>,
    /// ISO-8601 entry timestamp.
    pub timestamp: String,
    /// Unknown sibling fields.
    #[serde(flatten)]
    pub extra: Map<String, Value>,
}

/// Label bookmark entry targeting another entry.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct LabelEntry {
    #[serde(rename = "type")]
    kind: LabelTag,
    /// Short entry id.
    pub id: String,
    /// Parent entry id.
    pub parent_id: Option<String>,
    /// ISO-8601 entry timestamp.
    pub timestamp: String,
    /// Target entry id being labeled.
    pub target_id: String,
    /// Label text; absent/empty clears the label in the resolved map.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub label: Option<String>,
    /// Unknown sibling fields.
    #[serde(flatten)]
    pub extra: Map<String, Value>,
}

/// Session metadata entry (display name).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SessionInfoEntry {
    #[serde(rename = "type")]
    kind: SessionInfoTag,
    /// Short entry id.
    pub id: String,
    /// Parent entry id.
    pub parent_id: Option<String>,
    /// ISO-8601 entry timestamp.
    pub timestamp: String,
    /// Display name; empty/whitespace clears the session title.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    /// Unknown sibling fields.
    #[serde(flatten)]
    pub extra: Map<String, Value>,
}

// ---------------------------------------------------------------------------
// SessionEntry / FileEntry
// ---------------------------------------------------------------------------

/// A non-header session entry (tree node).
///
/// Unknown future discriminants land in [`SessionEntry::Unknown`] with the raw
/// JSON value so rewrites and forks never drop them. They contribute nothing to
/// LLM context but participate in the tree, leaf pointer, and id index when an
/// `id` field is present.
#[derive(Clone, Debug, PartialEq)]
pub enum SessionEntry {
    /// Transcript message.
    Message(SessionMessageEntry),
    /// Thinking-level change.
    ThinkingLevelChange(ThinkingLevelChangeEntry),
    /// Model change.
    ModelChange(ModelChangeEntry),
    /// Compaction summary.
    Compaction(CompactionEntry),
    /// Branch summary.
    BranchSummary(BranchSummaryEntry),
    /// Extension custom state.
    Custom(CustomEntry),
    /// Extension custom message (in context).
    CustomMessage(CustomMessageEntry),
    /// Label bookmark.
    Label(LabelEntry),
    /// Session display-name metadata.
    SessionInfo(SessionInfoEntry),
    /// Unknown or schema-invalid entry preserved as raw JSON.
    Unknown(Value),
}

impl SessionEntry {
    /// Wire `type` discriminant, or `""` when missing on an unknown entry.
    #[must_use]
    pub fn discriminant(&self) -> &str {
        match self {
            Self::Message(_) => "message",
            Self::ThinkingLevelChange(_) => "thinking_level_change",
            Self::ModelChange(_) => "model_change",
            Self::Compaction(_) => "compaction",
            Self::BranchSummary(_) => "branch_summary",
            Self::Custom(_) => "custom",
            Self::CustomMessage(_) => "custom_message",
            Self::Label(_) => "label",
            Self::SessionInfo(_) => "session_info",
            Self::Unknown(raw) => raw.get("type").and_then(Value::as_str).unwrap_or(""),
        }
    }

    /// Entry id when present as a string.
    #[must_use]
    pub fn id(&self) -> Option<&str> {
        match self {
            Self::Message(e) => Some(e.id.as_str()),
            Self::ThinkingLevelChange(e) => Some(e.id.as_str()),
            Self::ModelChange(e) => Some(e.id.as_str()),
            Self::Compaction(e) => Some(e.id.as_str()),
            Self::BranchSummary(e) => Some(e.id.as_str()),
            Self::Custom(e) => Some(e.id.as_str()),
            Self::CustomMessage(e) => Some(e.id.as_str()),
            Self::Label(e) => Some(e.id.as_str()),
            Self::SessionInfo(e) => Some(e.id.as_str()),
            Self::Unknown(raw) => raw.get("id").and_then(Value::as_str),
        }
    }

    /// Parent entry id when present as a string (`null`/missing → `None`).
    #[must_use]
    pub fn parent_id(&self) -> Option<&str> {
        match self {
            Self::Message(e) => e.parent_id.as_deref(),
            Self::ThinkingLevelChange(e) => e.parent_id.as_deref(),
            Self::ModelChange(e) => e.parent_id.as_deref(),
            Self::Compaction(e) => e.parent_id.as_deref(),
            Self::BranchSummary(e) => e.parent_id.as_deref(),
            Self::Custom(e) => e.parent_id.as_deref(),
            Self::CustomMessage(e) => e.parent_id.as_deref(),
            Self::Label(e) => e.parent_id.as_deref(),
            Self::SessionInfo(e) => e.parent_id.as_deref(),
            Self::Unknown(raw) => raw.get("parentId").and_then(Value::as_str),
        }
    }

    /// Entry timestamp when present as a string.
    #[must_use]
    pub fn timestamp(&self) -> Option<&str> {
        match self {
            Self::Message(e) => Some(e.timestamp.as_str()),
            Self::ThinkingLevelChange(e) => Some(e.timestamp.as_str()),
            Self::ModelChange(e) => Some(e.timestamp.as_str()),
            Self::Compaction(e) => Some(e.timestamp.as_str()),
            Self::BranchSummary(e) => Some(e.timestamp.as_str()),
            Self::Custom(e) => Some(e.timestamp.as_str()),
            Self::CustomMessage(e) => Some(e.timestamp.as_str()),
            Self::Label(e) => Some(e.timestamp.as_str()),
            Self::SessionInfo(e) => Some(e.timestamp.as_str()),
            Self::Unknown(raw) => raw.get("timestamp").and_then(Value::as_str),
        }
    }

    /// Set the entry id (used by create-branched rechain and tests).
    pub fn set_id(&mut self, id: String) {
        match self {
            Self::Message(e) => e.id = id,
            Self::ThinkingLevelChange(e) => e.id = id,
            Self::ModelChange(e) => e.id = id,
            Self::Compaction(e) => e.id = id,
            Self::BranchSummary(e) => e.id = id,
            Self::Custom(e) => e.id = id,
            Self::CustomMessage(e) => e.id = id,
            Self::Label(e) => e.id = id,
            Self::SessionInfo(e) => e.id = id,
            Self::Unknown(raw) => {
                if let Some(obj) = raw.as_object_mut() {
                    obj.insert("id".to_owned(), Value::String(id));
                }
            }
        }
    }

    /// Set the parent id (`None` → JSON `null`).
    pub fn set_parent_id(&mut self, parent_id: Option<String>) {
        match self {
            Self::Message(e) => e.parent_id = parent_id,
            Self::ThinkingLevelChange(e) => e.parent_id = parent_id,
            Self::ModelChange(e) => e.parent_id = parent_id,
            Self::Compaction(e) => e.parent_id = parent_id,
            Self::BranchSummary(e) => e.parent_id = parent_id,
            Self::Custom(e) => e.parent_id = parent_id,
            Self::CustomMessage(e) => e.parent_id = parent_id,
            Self::Label(e) => e.parent_id = parent_id,
            Self::SessionInfo(e) => e.parent_id = parent_id,
            Self::Unknown(raw) => {
                if let Some(obj) = raw.as_object_mut() {
                    obj.insert(
                        "parentId".to_owned(),
                        parent_id.map_or(Value::Null, Value::String),
                    );
                }
            }
        }
    }

    /// True when this entry is a message whose role is `"assistant"`.
    ///
    /// Unknown message-shaped raw entries are checked so deferred-write
    /// detection matches TypeScript (no validation on load).
    #[must_use]
    pub fn is_assistant_message(&self) -> bool {
        match self {
            Self::Message(e) => e.message.role() == "assistant",
            Self::Unknown(raw) => {
                raw.get("type").and_then(Value::as_str) == Some("message")
                    && raw
                        .get("message")
                        .and_then(|m| m.get("role"))
                        .and_then(Value::as_str)
                        == Some("assistant")
            }
            _ => false,
        }
    }

    /// Label target/label pair when this is a label entry.
    #[must_use]
    pub fn label_fields(&self) -> Option<(Option<&str>, Option<&str>)> {
        match self {
            Self::Label(e) => Some((Some(e.target_id.as_str()), e.label.as_deref())),
            Self::Unknown(raw) if raw.get("type").and_then(Value::as_str) == Some("label") => {
                Some((
                    raw.get("targetId").and_then(Value::as_str),
                    raw.get("label").and_then(Value::as_str),
                ))
            }
            _ => None,
        }
    }

    /// Session-info name when this is a `session_info` entry.
    #[must_use]
    pub fn session_info_name(&self) -> Option<Option<&str>> {
        match self {
            Self::SessionInfo(e) => Some(e.name.as_deref()),
            Self::Unknown(raw)
                if raw.get("type").and_then(Value::as_str) == Some("session_info") =>
            {
                Some(raw.get("name").and_then(Value::as_str))
            }
            _ => None,
        }
    }
}

impl Serialize for SessionEntry {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        match self {
            Self::Message(e) => e.serialize(serializer),
            Self::ThinkingLevelChange(e) => e.serialize(serializer),
            Self::ModelChange(e) => e.serialize(serializer),
            Self::Compaction(e) => e.serialize(serializer),
            Self::BranchSummary(e) => e.serialize(serializer),
            Self::Custom(e) => e.serialize(serializer),
            Self::CustomMessage(e) => e.serialize(serializer),
            Self::Label(e) => e.serialize(serializer),
            Self::SessionInfo(e) => e.serialize(serializer),
            Self::Unknown(raw) => raw.serialize(serializer),
        }
    }
}

impl<'de> Deserialize<'de> for SessionEntry {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let value = Value::deserialize(deserializer)?;
        Ok(session_entry_from_value(value))
    }
}

/// Raw file line: header or tree entry.
#[derive(Clone, Debug, PartialEq)]
pub enum FileEntry {
    /// Typed session header.
    Header(SessionHeader),
    /// A `type: "session"` line that failed typed header parsing (preserved).
    RawHeader(Value),
    /// Non-header tree entry (typed or unknown).
    Entry(SessionEntry),
}

impl FileEntry {
    /// True when this line is a session header (`type: "session"`).
    #[must_use]
    pub const fn is_session_header(&self) -> bool {
        matches!(self, Self::Header(_) | Self::RawHeader(_))
    }

    /// Typed header when present.
    #[must_use]
    pub const fn header(&self) -> Option<&SessionHeader> {
        match self {
            Self::Header(h) => Some(h),
            _ => None,
        }
    }

    /// Tree entry when present.
    #[must_use]
    pub const fn entry(&self) -> Option<&SessionEntry> {
        match self {
            Self::Entry(e) => Some(e),
            _ => None,
        }
    }

    /// Mutable tree entry when present.
    pub fn entry_mut(&mut self) -> Option<&mut SessionEntry> {
        match self {
            Self::Entry(e) => Some(e),
            _ => None,
        }
    }
}

impl Serialize for FileEntry {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        match self {
            Self::Header(h) => h.serialize(serializer),
            Self::RawHeader(raw) => raw.serialize(serializer),
            Self::Entry(e) => e.serialize(serializer),
        }
    }
}

impl<'de> Deserialize<'de> for FileEntry {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let value = Value::deserialize(deserializer)?;
        Ok(file_entry_from_value(value))
    }
}

// ---------------------------------------------------------------------------
// Parse / load
// ---------------------------------------------------------------------------

/// Parse one JSONL line into a [`FileEntry`]. Empty / malformed lines → `None`.
#[must_use]
pub fn parse_session_entry_line(line: &str) -> Option<FileEntry> {
    if line.trim().is_empty() {
        return None;
    }
    let value: Value = serde_json::from_str(line).ok()?;
    Some(file_entry_from_value(value))
}

/// Parse session content into file entries (no header validation).
///
/// Mirrors TypeScript `parseSessionEntries`: trim content, split on lines, skip
/// empty and unparseable lines.
#[must_use]
pub fn parse_session_entries(content: &str) -> Vec<FileEntry> {
    let mut entries = Vec::new();
    for line in content.trim().lines() {
        if let Some(entry) = parse_session_entry_line(line) {
            entries.push(entry);
        }
    }
    entries
}

/// Load entries from a session file.
///
/// Returns an empty vec when the file is missing, empty, unparseable, or lacks
/// a valid session header (first parseable entry must be `type: "session"` with
/// a string `id`). Malformed mid-file lines are skipped.
#[must_use]
pub fn load_entries_from_file(file_path: &Path) -> Vec<FileEntry> {
    match load_values_from_file(file_path) {
        Ok(values) => values.into_iter().map(file_entry_from_value).collect(),
        Err(_) => Vec::new(),
    }
}

/// Load raw JSON values from a session file (header-validated).
///
/// Missing file → empty vec. Non-empty file without a valid header → empty
/// vec (caller distinguishes via size for the invalid-file error). IO errors
/// other than `NotFound` are returned.
pub(crate) fn load_values_from_file(file_path: &Path) -> io::Result<Vec<Value>> {
    let normalized = normalize_path(&file_path.to_string_lossy(), PathInputOptions::new());
    if !path_exists(&normalized) {
        return Ok(Vec::new());
    }

    let file = File::open(&normalized)?;
    let mut values = Vec::new();
    for chunk in BufReader::new(file).split(b'\n') {
        let bytes = chunk?;
        let line = String::from_utf8_lossy(&bytes);
        if line.trim().is_empty() {
            continue;
        }
        if let Ok(value) = serde_json::from_str::<Value>(&line) {
            values.push(value);
        }
    }

    if values.is_empty() {
        return Ok(values);
    }

    let header = &values[0];
    let is_session = header.get("type").and_then(Value::as_str) == Some("session");
    let has_string_id = header.get("id").is_some_and(Value::is_string);
    if !is_session || !has_string_id {
        return Ok(Vec::new());
    }

    Ok(values)
}

/// Read the first-line session header from a file (≤512-byte probe).
///
/// Returns `None` when the file is unreadable, the first line is not a session
/// header, or `id` is not a string.
#[must_use]
pub fn read_session_header(file_path: &Path) -> Option<SessionHeader> {
    let mut file = File::open(file_path).ok()?;
    let mut buf = vec![0_u8; SESSION_HEADER_PROBE_BYTES];
    let n = file.read(&mut buf).ok()?;
    let text = String::from_utf8_lossy(&buf[..n]);
    let first_line = text.split('\n').next()?;
    if first_line.is_empty() {
        return None;
    }
    let value: Value = serde_json::from_str(first_line).ok()?;
    if value.get("type").and_then(Value::as_str) != Some("session") {
        return None;
    }
    if !value.get("id").is_some_and(Value::is_string) {
        return None;
    }
    serde_json::from_value(value).ok()
}

// ---------------------------------------------------------------------------
// Migrations
// ---------------------------------------------------------------------------

/// Run v1→v2→v3 migrations on raw entry values. Mutates in place.
///
/// Returns `true` when any migration was applied. Exported for tests via
/// [`migrate_session_entries`].
pub(crate) fn migrate_values_to_current(values: &mut [Value]) -> bool {
    let version = values
        .iter()
        .find(|v| v.get("type").and_then(Value::as_str) == Some("session"))
        .and_then(|h| h.get("version").and_then(Value::as_u64))
        .unwrap_or(1);

    if version >= CURRENT_SESSION_VERSION {
        return false;
    }

    if version < 2 {
        migrate_v1_to_v2(values);
    }
    // After v1→v2, version is 2; after only v2 path, version is still the original.
    // Re-check from the (possibly mutated) header.
    let version_after = values
        .iter()
        .find(|v| v.get("type").and_then(Value::as_str) == Some("session"))
        .and_then(|h| h.get("version").and_then(Value::as_u64))
        .unwrap_or(1);
    if version_after < 3 {
        migrate_v2_to_v3(values);
    }
    true
}

/// Exported test hook: migrate typed file entries in place (round-trips via JSON).
pub fn migrate_session_entries(entries: &mut Vec<FileEntry>) {
    let mut values: Vec<Value> = entries
        .iter()
        .filter_map(|e| serde_json::to_value(e).ok())
        .collect();
    if values.len() != entries.len() {
        return;
    }
    let _ = migrate_values_to_current(&mut values);
    *entries = values.into_iter().map(file_entry_from_value).collect();
}

fn migrate_v1_to_v2(values: &mut [Value]) {
    let mut ids: HashSet<String> = HashSet::new();
    let mut prev_id: Option<String> = None;

    for i in 0..values.len() {
        let is_session = values[i].get("type").and_then(Value::as_str) == Some("session");
        if is_session {
            if let Some(obj) = values[i].as_object_mut() {
                obj.insert("version".to_owned(), Value::from(2_u64));
            }
            continue;
        }

        if !values[i].is_object() {
            continue;
        }

        let id = generate_id(|c| ids.contains(c));
        ids.insert(id.clone());

        // Assign id + parentId first so a self-referencing compaction index
        // sees the just-assigned id (mirrors TS loop order).
        if let Some(obj) = values[i].as_object_mut() {
            obj.insert("id".to_owned(), Value::String(id.clone()));
            obj.insert(
                "parentId".to_owned(),
                prev_id.clone().map_or(Value::Null, Value::String),
            );
        }
        prev_id = Some(id);

        // Compaction: firstKeptEntryIndex (number) → firstKeptEntryId.
        if values[i].get("type").and_then(Value::as_str) == Some("compaction")
            && values[i]
                .get("firstKeptEntryIndex")
                .is_some_and(Value::is_number)
        {
            let idx = values[i]
                .get("firstKeptEntryIndex")
                .and_then(json_number_as_usize);
            let target_id = idx.and_then(|j| {
                let target = values.get(j)?;
                if target.get("type").and_then(Value::as_str) == Some("session") {
                    return None;
                }
                target.get("id").and_then(Value::as_str).map(str::to_owned)
            });
            if let Some(obj) = values[i].as_object_mut() {
                if let Some(tid) = target_id {
                    obj.insert("firstKeptEntryId".to_owned(), Value::String(tid));
                }
                obj.remove("firstKeptEntryIndex");
            }
        }
    }
}

fn migrate_v2_to_v3(values: &mut [Value]) {
    for value in values.iter_mut() {
        let is_session = value.get("type").and_then(Value::as_str) == Some("session");
        if is_session {
            if let Some(obj) = value.as_object_mut() {
                obj.insert("version".to_owned(), Value::from(3_u64));
            }
            continue;
        }

        if value.get("type").and_then(Value::as_str) != Some("message") {
            continue;
        }
        let Some(message) = value.get_mut("message") else {
            continue;
        };
        if message.get("role").and_then(Value::as_str) == Some("hookMessage")
            && let Some(obj) = message.as_object_mut()
        {
            obj.insert("role".to_owned(), Value::String("custom".to_owned()));
        }
    }
}

fn json_number_as_usize(value: &Value) -> Option<usize> {
    if let Some(u) = value.as_u64() {
        return usize::try_from(u).ok();
    }
    if let Some(i) = value.as_i64() {
        return usize::try_from(i).ok();
    }
    // Preserve TS float-index edge (`1.0`) without f64→usize casts.
    let f = value.as_f64()?;
    if f.is_finite() && f >= 0.0 && f.fract() == 0.0 {
        format!("{f:.0}").parse::<usize>().ok()
    } else {
        None
    }
}

// ---------------------------------------------------------------------------
// ID / timestamp helpers
// ---------------------------------------------------------------------------

/// Generate a unique short entry id (8 hex chars, collision-checked).
///
/// Falls back to a full UUID after 100 collisions.
#[must_use]
pub fn generate_id(has: impl Fn(&str) -> bool) -> String {
    for _ in 0..100 {
        let full = Uuid::new_v4().simple().to_string();
        let id = &full[..8];
        if !has(id) {
            return id.to_owned();
        }
    }
    Uuid::new_v4().to_string()
}

/// Create a `UUIDv7` session id.
#[must_use]
pub fn create_session_id() -> String {
    Uuid::now_v7().to_string()
}

/// Validate a custom session id (alphanumeric with interior `._-`).
///
/// Error text matches TypeScript `assertValidSessionId` exactly when invalid.
///
/// # Errors
///
/// Returns [`super::SessionError::InvalidSessionId`] when `id` is empty, has
/// non-alphanumeric edges, or contains characters outside `[A-Za-z0-9._-]`.
pub fn assert_valid_session_id(id: &str) -> Result<(), super::SessionError> {
    if is_valid_session_id(id) {
        Ok(())
    } else {
        Err(super::SessionError::InvalidSessionId)
    }
}

fn is_valid_session_id(id: &str) -> bool {
    let bytes = id.as_bytes();
    if bytes.is_empty() {
        return false;
    }
    let is_alnum = |b: u8| b.is_ascii_alphanumeric();
    let is_inner = |b: u8| is_alnum(b) || matches!(b, b'.' | b'_' | b'-');
    is_alnum(bytes[0]) && is_alnum(bytes[bytes.len() - 1]) && bytes.iter().copied().all(is_inner)
}

/// Current wall-clock time as an ISO-8601 string with exactly 3 fractional digits and `Z`.
#[must_use]
pub fn now_iso() -> String {
    iso_from_millis(now_millis())
}

/// Current Unix timestamp in milliseconds.
#[must_use]
pub fn now_millis() -> i64 {
    jiff::Timestamp::now().as_millisecond()
}

/// Format millisecond epoch as `YYYY-MM-DDTHH:MM:SS.mmmZ`.
#[must_use]
pub fn iso_from_millis(ms: i64) -> String {
    let ts = jiff::Timestamp::from_millisecond(ms).unwrap_or(jiff::Timestamp::UNIX_EPOCH);
    let dt = ts.to_zoned(jiff::tz::TimeZone::UTC).datetime();
    format!(
        "{:04}-{:02}-{:02}T{:02}:{:02}:{:02}.{:03}Z",
        dt.year(),
        dt.month(),
        dt.day(),
        dt.hour(),
        dt.minute(),
        dt.second(),
        dt.millisecond()
    )
}

/// Parse an ISO-8601 / RFC-3339 timestamp to Unix milliseconds.
#[must_use]
pub fn iso_to_millis(timestamp: &str) -> Option<i64> {
    timestamp
        .parse::<jiff::Timestamp>()
        .ok()
        .map(jiff::Timestamp::as_millisecond)
}

/// [`SystemTime`] → Unix milliseconds (handles pre-epoch).
#[must_use]
pub fn system_time_millis(t: SystemTime) -> i64 {
    match t.duration_since(UNIX_EPOCH) {
        Ok(d) => i64::try_from(d.as_millis()).unwrap_or(i64::MAX),
        Err(e) => -i64::try_from(e.duration().as_millis()).unwrap_or(i64::MAX),
    }
}

/// Metadata mtime as Unix milliseconds.
#[must_use]
pub fn mtime_millis(meta: &std::fs::Metadata) -> Option<i64> {
    meta.modified().ok().map(system_time_millis)
}

/// Path existence check matching TypeScript `existsSync` (errors → false).
#[must_use]
pub fn path_exists(path: &Path) -> bool {
    path.try_exists().unwrap_or(false)
}

// ---------------------------------------------------------------------------
// Internal parse helpers
// ---------------------------------------------------------------------------

pub(crate) fn file_entry_from_value(value: Value) -> FileEntry {
    match value.get("type").and_then(Value::as_str) {
        Some("session") => match serde_json::from_value::<SessionHeader>(value.clone()) {
            Ok(h) => FileEntry::Header(h),
            Err(_) => FileEntry::RawHeader(value),
        },
        _ => FileEntry::Entry(session_entry_from_value(value)),
    }
}

fn session_entry_from_value(value: Value) -> SessionEntry {
    let ty = value.get("type").and_then(Value::as_str);
    match ty {
        Some("message") => {
            let mut patched = value.clone();
            if let Some(msg) = patched.get_mut("message").and_then(Value::as_object_mut) {
                let role = msg.get("role").and_then(Value::as_str);
                if matches!(role, Some("user" | "assistant" | "toolResult"))
                    && matches!(msg.get("content"), None | Some(Value::Null))
                {
                    msg.insert("content".to_owned(), Value::Array(Vec::new()));
                }
            }
            match serde_json::from_value::<SessionMessageEntry>(patched) {
                Ok(e) => SessionEntry::Message(e),
                Err(_) => SessionEntry::Unknown(value),
            }
        }
        Some("thinking_level_change") => {
            match serde_json::from_value::<ThinkingLevelChangeEntry>(value.clone()) {
                Ok(e) => SessionEntry::ThinkingLevelChange(e),
                Err(_) => SessionEntry::Unknown(value),
            }
        }
        Some("model_change") => match serde_json::from_value::<ModelChangeEntry>(value.clone()) {
            Ok(e) => SessionEntry::ModelChange(e),
            Err(_) => SessionEntry::Unknown(value),
        },
        Some("compaction") => match serde_json::from_value::<CompactionEntry>(value.clone()) {
            Ok(e) => SessionEntry::Compaction(e),
            Err(_) => SessionEntry::Unknown(value),
        },
        Some("branch_summary") => {
            match serde_json::from_value::<BranchSummaryEntry>(value.clone()) {
                Ok(e) => SessionEntry::BranchSummary(e),
                Err(_) => SessionEntry::Unknown(value),
            }
        }
        Some("custom") => match serde_json::from_value::<CustomEntry>(value.clone()) {
            Ok(e) => SessionEntry::Custom(e),
            Err(_) => SessionEntry::Unknown(value),
        },
        Some("custom_message") => {
            let mut patched = value.clone();
            if let Some(obj) = patched.as_object_mut()
                && matches!(obj.get("content"), None | Some(Value::Null))
            {
                obj.insert("content".to_owned(), Value::Array(Vec::new()));
            }
            match serde_json::from_value::<CustomMessageEntry>(patched) {
                Ok(e) => SessionEntry::CustomMessage(e),
                Err(_) => SessionEntry::Unknown(value),
            }
        }
        Some("label") => match serde_json::from_value::<LabelEntry>(value.clone()) {
            Ok(e) => SessionEntry::Label(e),
            Err(_) => SessionEntry::Unknown(value),
        },
        Some("session_info") => match serde_json::from_value::<SessionInfoEntry>(value.clone()) {
            Ok(e) => SessionEntry::SessionInfo(e),
            Err(_) => SessionEntry::Unknown(value),
        },
        _ => SessionEntry::Unknown(value),
    }
}

/// Serialize a file entry to a single JSONL line (no trailing newline).
pub(crate) fn file_entry_to_line(entry: &FileEntry) -> Result<String, serde_json::Error> {
    serde_json::to_string(entry)
}

/// Serialize a session entry to a single JSONL line (no trailing newline).
pub(crate) fn session_entry_to_line(entry: &SessionEntry) -> Result<String, serde_json::Error> {
    serde_json::to_string(entry)
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    type TestResult = Result<(), Box<dyn std::error::Error>>;

    #[test]
    fn migrate_v1_assigns_ids_and_sets_version_3() -> TestResult {
        let mut entries: Vec<FileEntry> = vec![
            file_entry_from_value(json!({
                "type": "session",
                "id": "sess-1",
                "timestamp": "2025-01-01T00:00:00Z",
                "cwd": "/tmp"
            })),
            file_entry_from_value(json!({
                "type": "message",
                "timestamp": "2025-01-01T00:00:01Z",
                "message": { "role": "user", "content": "hi", "timestamp": 1 }
            })),
            file_entry_from_value(json!({
                "type": "message",
                "timestamp": "2025-01-01T00:00:02Z",
                "message": {
                    "role": "assistant",
                    "content": [{ "type": "text", "text": "hello" }],
                    "api": "test",
                    "provider": "test",
                    "model": "test",
                    "usage": {
                        "input": 1, "output": 1, "cacheRead": 0, "cacheWrite": 0,
                        "totalTokens": 2,
                        "cost": { "input": 0, "output": 0, "cacheRead": 0, "cacheWrite": 0, "total": 0 }
                    },
                    "stopReason": "stop",
                    "timestamp": 2
                }
            })),
        ];

        migrate_session_entries(&mut entries);

        let header = entries[0].header().ok_or("expected header")?;
        assert_eq!(header.version, Some(3));

        let msg1 = entries[1].entry().ok_or("expected entry")?;
        let msg2 = entries[2].entry().ok_or("expected entry")?;
        let id1 = msg1.id().ok_or("id1")?;
        let id2 = msg2.id().ok_or("id2")?;
        assert_eq!(id1.len(), 8);
        assert_eq!(id2.len(), 8);
        assert!(msg1.parent_id().is_none());
        assert_eq!(msg2.parent_id(), Some(id1));
        Ok(())
    }

    #[test]
    fn migrate_v2_is_idempotent_on_ids() {
        let mut entries: Vec<FileEntry> = vec![
            file_entry_from_value(json!({
                "type": "session",
                "id": "sess-1",
                "version": 2,
                "timestamp": "2025-01-01T00:00:00Z",
                "cwd": "/tmp"
            })),
            file_entry_from_value(json!({
                "type": "message",
                "id": "abc12345",
                "parentId": null,
                "timestamp": "2025-01-01T00:00:01Z",
                "message": { "role": "user", "content": "hi", "timestamp": 1 }
            })),
            file_entry_from_value(json!({
                "type": "message",
                "id": "def67890",
                "parentId": "abc12345",
                "timestamp": "2025-01-01T00:00:02Z",
                "message": {
                    "role": "assistant",
                    "content": [{ "type": "text", "text": "hello" }],
                    "api": "test",
                    "provider": "test",
                    "model": "test",
                    "usage": {
                        "input": 1, "output": 1, "cacheRead": 0, "cacheWrite": 0,
                        "totalTokens": 2,
                        "cost": { "input": 0, "output": 0, "cacheRead": 0, "cacheWrite": 0, "total": 0 }
                    },
                    "stopReason": "stop",
                    "timestamp": 2
                }
            })),
        ];

        migrate_session_entries(&mut entries);

        assert_eq!(
            entries[1].entry().and_then(SessionEntry::id),
            Some("abc12345")
        );
        assert_eq!(
            entries[2].entry().and_then(SessionEntry::id),
            Some("def67890")
        );
        assert_eq!(
            entries[2].entry().and_then(SessionEntry::parent_id),
            Some("abc12345")
        );
        assert_eq!(entries[0].header().and_then(|h| h.version), Some(3));
    }

    #[test]
    fn migrate_v2_renames_hook_message_to_custom() {
        let mut values = vec![
            json!({
                "type": "session",
                "id": "s",
                "version": 2,
                "timestamp": "2025-01-01T00:00:00Z",
                "cwd": "/tmp"
            }),
            json!({
                "type": "message",
                "id": "a1",
                "parentId": null,
                "timestamp": "2025-01-01T00:00:01Z",
                "message": {
                    "role": "hookMessage",
                    "customType": "x",
                    "content": "hi",
                    "display": true,
                    "timestamp": 1
                }
            }),
        ];
        assert!(migrate_values_to_current(&mut values));
        assert_eq!(values[0]["version"], json!(3));
        assert_eq!(values[1]["message"]["role"], json!("custom"));
    }

    #[test]
    fn unknown_entry_round_trips() -> TestResult {
        let raw = json!({
            "type": "future_thing",
            "id": "u1",
            "parentId": null,
            "timestamp": "2025-01-01T00:00:00Z",
            "customField": { "a": 1 }
        });
        let entry = session_entry_from_value(raw.clone());
        assert!(matches!(entry, SessionEntry::Unknown(_)));
        assert_eq!(entry.id(), Some("u1"));
        assert_eq!(entry.discriminant(), "future_thing");
        let line = session_entry_to_line(&entry)?;
        let reparsed: Value = serde_json::from_str(&line)?;
        assert_eq!(reparsed, raw);
        Ok(())
    }

    #[test]
    fn null_content_message_loads_as_typed() -> TestResult {
        let raw = json!({
            "type": "message",
            "id": "m1",
            "parentId": null,
            "timestamp": "2025-01-01T00:00:00Z",
            "message": { "role": "user", "content": null, "timestamp": 1 }
        });
        let entry = session_entry_from_value(raw);
        match entry {
            SessionEntry::Message(m) => {
                assert_eq!(m.message.role(), "user");
            }
            other => return Err(format!("expected Message, got {other:?}").into()),
        }
        Ok(())
    }

    #[test]
    fn session_id_validation() {
        assert!(assert_valid_session_id("abc").is_ok());
        assert!(assert_valid_session_id("abc-123_def.456").is_ok());
        assert!(assert_valid_session_id("a").is_ok());
        for id in [
            "", "-abc", "abc-", "_abc", "abc_", ".abc", "abc.", "abc/def", "abc\\def", "abc def",
        ] {
            assert!(
                assert_valid_session_id(id).is_err(),
                "expected invalid: {id:?}"
            );
        }
    }

    #[test]
    fn iso_format_has_three_fractional_digits() {
        let iso = now_iso();
        let re = regex_like_iso(&iso);
        assert!(re, "unexpected iso format: {iso}");
        // Round-trip through jiff
        assert!(iso_to_millis(&iso).is_some());
    }

    fn regex_like_iso(s: &str) -> bool {
        // YYYY-MM-DDTHH:MM:SS.mmmZ
        let b = s.as_bytes();
        if b.len() != 24 {
            return false;
        }
        b[4] == b'-'
            && b[7] == b'-'
            && b[10] == b'T'
            && b[13] == b':'
            && b[16] == b':'
            && b[19] == b'.'
            && b[23] == b'Z'
            && b[..4].iter().all(u8::is_ascii_digit)
            && b[20..23].iter().all(u8::is_ascii_digit)
    }

    #[test]
    fn uuid_v7_session_id_shape() {
        let id = create_session_id();
        // xxxxxxxx-xxxx-7xxx-[89ab]xxx-xxxxxxxxxxxx
        let parts: Vec<&str> = id.split('-').collect();
        assert_eq!(parts.len(), 5);
        assert_eq!(parts[0].len(), 8);
        assert_eq!(parts[1].len(), 4);
        assert_eq!(parts[2].len(), 4);
        assert!(parts[2].starts_with('7'));
        assert_eq!(parts[3].len(), 4);
        assert!(matches!(parts[3].as_bytes()[0], b'8' | b'9' | b'a' | b'b'));
        assert_eq!(parts[4].len(), 12);
    }

    #[test]
    fn generate_id_is_eight_hex() {
        let id = generate_id(|_| false);
        assert_eq!(id.len(), 8);
        assert!(id.chars().all(|c| c.is_ascii_hexdigit()));
    }

    #[test]
    fn parse_session_entries_skips_malformed() {
        let content = r#"{"type":"session","id":"abc","timestamp":"2025-01-01T00:00:00Z","cwd":"/tmp"}
not valid json
{"type":"message","id":"1","parentId":null,"timestamp":"2025-01-01T00:00:01Z","message":{"role":"user","content":"hi","timestamp":1}}
"#;
        let entries = parse_session_entries(content);
        assert_eq!(entries.len(), 2);
        assert!(entries[0].is_session_header());
        assert_eq!(entries[1].entry().and_then(SessionEntry::id), Some("1"));
    }

    #[test]
    fn load_nonexistent_returns_empty() {
        let entries =
            load_entries_from_file(Path::new("/tmp/pi-oxidized-no-such-session-file.jsonl"));
        assert!(entries.is_empty());
    }
}
