//! Append-only JSONL v3 session store.
//!
//! Ports `.references/pi/packages/coding-agent/src/core/session-manager.ts` and
//! `session-cwd.ts`. Disk create is deferred until the first assistant message
//! (`create_new` / `wx`); historical lines are rewritten only on migration load,
//! empty-file init, or [`SessionManager::create_branched_session`].

pub mod context;
pub mod cwd;
pub mod entries;
pub mod list;

use std::collections::{HashMap, HashSet};
use std::fs::{self, File, OpenOptions};
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use pi_agent::AgentMessage;
use serde_json::Value;
use thiserror::Error;

use super::config::{
    PathInputOptions, get_agent_dir, get_sessions_dir, normalize_path, resolve_path,
};
use super::messages::{CustomMessageContent, MessageConversionError};

pub use context::{
    DEFAULT_THINKING_LEVEL, LeafRef, SessionContext, SessionModel, build_context_entries,
    build_session_context, build_session_path, get_latest_compaction_entry,
    session_entry_to_context_messages,
};
pub use cwd::{
    MissingSessionCwdError, SessionCwdIssue, SessionCwdSource, assert_session_cwd_exists,
    format_missing_session_cwd_error, format_missing_session_cwd_prompt,
    get_missing_session_cwd_issue,
};
pub use entries::{
    BranchSummaryEntry, CURRENT_SESSION_VERSION, CompactionEntry, CustomEntry, CustomMessageEntry,
    FileEntry, LabelEntry, ModelChangeEntry, NO_MESSAGES_PLACEHOLDER, SessionEntry, SessionHeader,
    SessionInfoEntry, SessionMessageEntry, ThinkingLevelChangeEntry, assert_valid_session_id,
    create_session_id, generate_id, load_entries_from_file, migrate_session_entries, now_iso,
    parse_session_entries, parse_session_entry_line, read_session_header,
};
pub use list::{
    MAX_CONCURRENT_SESSION_INFO_LOADS, SessionInfo, SessionListProgress, build_session_info,
    find_most_recent_session, list_all_sessions, list_sessions_for_cwd, list_sessions_from_dir,
    session_cwd_matches,
};

use entries::{
    file_entry_to_line, iso_to_millis, load_file_entries_from_file, load_values_from_file,
    migrate_values_to_current, path_exists, session_entry_to_line,
};

// ---------------------------------------------------------------------------
// Errors
// ---------------------------------------------------------------------------

/// Errors from session manager operations.
#[derive(Debug, Error)]
pub enum SessionError {
    /// Custom session id failed validation.
    #[error(
        "Session id must be non-empty, contain only alphanumeric characters, '-', '_', and '.', and start and end with an alphanumeric character"
    )]
    InvalidSessionId,
    /// Referenced entry id is not in the tree.
    #[error("Entry {0} not found")]
    EntryNotFound(String),
    /// Non-empty file that is not a valid pi session.
    #[error("Session file is not a valid pi session: {0}")]
    InvalidSessionFile(String),
    /// Fork source is empty or invalid.
    #[error("Cannot fork: source session file is empty or invalid: {0}")]
    ForkSourceEmpty(String),
    /// Fork source has no session header.
    #[error("Cannot fork: source session has no header: {0}")]
    ForkSourceNoHeader(String),
    /// Filesystem IO failure.
    #[error("I/O error on {path}: {source}")]
    Io {
        /// Path involved in the failure.
        path: String,
        /// Underlying IO error.
        source: io::Error,
    },
    /// JSON serialization failure.
    #[error("JSON error: {0}")]
    Json(#[from] serde_json::Error),
    /// Context projection / message conversion failure.
    #[error(transparent)]
    Message(#[from] MessageConversionError),
}

// assert_valid_session_id in entries.rs returns SessionError defined above.

// ---------------------------------------------------------------------------
// Options / tree node
// ---------------------------------------------------------------------------

/// Options for creating a new session.
#[derive(Clone, Debug, Default)]
pub struct NewSessionOptions {
    /// Optional custom session id (validated).
    pub id: Option<String>,
    /// Optional parent session file path stored in the header.
    pub parent_session: Option<String>,
}

/// Tree node returned by [`SessionManager::get_tree`].
#[derive(Clone, Debug, PartialEq)]
pub struct SessionTreeNode {
    /// Entry at this node.
    pub entry: SessionEntry,
    /// Children sorted by timestamp ascending.
    pub children: Vec<SessionTreeNode>,
    /// Resolved label for this entry, if any.
    pub label: Option<String>,
    /// Timestamp of the latest label change for this entry, if any.
    pub label_timestamp: Option<String>,
}

// ---------------------------------------------------------------------------
// Leaf + labels
// ---------------------------------------------------------------------------

#[derive(Clone, Debug)]
enum Leaf {
    Null,
    Id(String),
    /// Last non-session entry had no string id (TS `undefined` leaf).
    Tail,
}

/// Insertion-ordered label map (`target_id` → label + timestamp).
#[derive(Clone, Debug, Default)]
struct LabelMap {
    /// Insertion-ordered (target, label, timestamp).
    entries: Vec<(String, String, String)>,
    /// target → index into `entries`.
    index: HashMap<String, usize>,
}

impl LabelMap {
    fn clear(&mut self) {
        self.entries.clear();
        self.index.clear();
    }

    fn get(&self, target: &str) -> Option<&str> {
        self.index.get(target).map(|&i| self.entries[i].1.as_str())
    }

    fn get_timestamp(&self, target: &str) -> Option<&str> {
        self.index.get(target).map(|&i| self.entries[i].2.as_str())
    }

    /// Set or update a label. Existing targets keep their insertion position.
    fn set(&mut self, target: String, label: String, timestamp: String) {
        if let Some(&i) = self.index.get(&target) {
            self.entries[i].1 = label;
            self.entries[i].2 = timestamp;
        } else {
            let i = self.entries.len();
            self.index.insert(target.clone(), i);
            self.entries.push((target, label, timestamp));
        }
    }

    fn delete(&mut self, target: &str) {
        let Some(i) = self.index.remove(target) else {
            return;
        };
        self.entries.remove(i);
        // Rebuild index after removal.
        self.index.clear();
        for (idx, (t, _, _)) in self.entries.iter().enumerate() {
            self.index.insert(t.clone(), idx);
        }
    }

    fn iter(&self) -> impl Iterator<Item = (&str, &str, &str)> {
        self.entries
            .iter()
            .map(|(t, l, ts)| (t.as_str(), l.as_str(), ts.as_str()))
    }
}

// ---------------------------------------------------------------------------
// SessionManager
// ---------------------------------------------------------------------------

/// Append-only JSONL v3 session tree.
///
/// Single-writer, no interior locking (matches TypeScript). Historical lines are
/// never rewritten on normal append.
#[derive(Debug)]
pub struct SessionManager {
    session_id: String,
    session_file: Option<String>,
    session_dir: String,
    cwd: String,
    persist: bool,
    flushed: bool,
    file_entries: Vec<FileEntry>,
    /// id → index into `file_entries` (Entry variants only).
    by_id: HashMap<String, usize>,
    labels: LabelMap,
    leaf: Leaf,
}

fn assemble_tree_node(
    id: &str,
    node_map: &HashMap<String, SessionTreeNode>,
    children_of: &HashMap<String, Vec<String>>,
) -> Option<SessionTreeNode> {
    let mut node = node_map.get(id)?.clone();
    node.children.clear();
    if let Some(kids) = children_of.get(id) {
        for kid in kids {
            if let Some(child) = assemble_tree_node(kid, node_map, children_of) {
                node.children.push(child);
            }
        }
        node.children.sort_by(|a, b| {
            let ta = a.entry.timestamp().and_then(iso_to_millis);
            let tb = b.entry.timestamp().and_then(iso_to_millis);
            match (ta, tb) {
                (Some(x), Some(y)) => x.cmp(&y),
                _ => std::cmp::Ordering::Equal,
            }
        });
    }
    Some(node)
}

impl SessionManager {
    fn construct(
        cwd: &str,
        session_dir: &str,
        session_file: Option<String>,
        persist: bool,
        options: Option<NewSessionOptions>,
    ) -> Result<Self, SessionError> {
        let mut sm = Self::construct_empty(cwd, session_dir, persist)?;
        if let Some(file) = session_file {
            sm.set_session_file(&file)?;
        } else {
            sm.new_session(options)?;
        }
        Ok(sm)
    }

    /// Shared constructor body: resolved cwd/dir, session-dir creation, and
    /// the empty manager that the file-loading entry points
    /// ([`Self::set_session_file`], [`Self::open`]) then populate.
    fn construct_empty(
        cwd: &str,
        session_dir: &str,
        persist: bool,
    ) -> Result<Self, SessionError> {
        let cwd = path_to_string(&resolve_path(cwd));
        let session_dir = path_to_string(&normalize_path(session_dir, PathInputOptions::new()));
        if persist && !session_dir.is_empty() && !path_exists(Path::new(&session_dir)) {
            fs::create_dir_all(&session_dir).map_err(|source| SessionError::Io {
                path: session_dir.clone(),
                source,
            })?;
        }
        Ok(Self {
            session_id: String::new(),
            session_file: None,
            session_dir,
            cwd,
            persist,
            flushed: false,
            file_entries: Vec::new(),
            by_id: HashMap::new(),
            labels: LabelMap::default(),
            leaf: Leaf::Null,
        })
    }

    /// Switch to a different session file (resume / open).
    ///
    /// # Errors
    ///
    /// Returns IO errors when reading or rewriting the file, invalid-session when a
    /// non-empty file is not a pi session, and JSON errors during migration.
    pub fn set_session_file(&mut self, session_file: &str) -> Result<(), SessionError> {
        let resolved = path_to_string(&resolve_path(session_file));
        self.session_file = Some(resolved.clone());

        if path_exists(Path::new(&resolved)) {
            let entries =
                load_file_entries_from_file(Path::new(&resolved)).map_err(|source| {
                    SessionError::Io {
                        path: resolved.clone(),
                        source,
                    }
                })?;
            self.apply_session_file_entries(resolved, entries)
        } else {
            self.new_session(None)?;
            self.session_file = Some(resolved);
            Ok(())
        }
    }

    /// Bind the manager state to an already-loaded session file's entries.
    ///
    /// Shared by [`Self::set_session_file`] and [`Self::open`] so a reopen
    /// parses the file exactly once. The branches mirror the historical
    /// post-load behavior exactly: empty-file init + rewrite, header id
    /// extraction, the version gate with the legacy Value + migration lane,
    /// and the index rebuild.
    fn apply_session_file_entries(
        &mut self,
        resolved: String,
        entries: Vec<FileEntry>,
    ) -> Result<(), SessionError> {
        if entries.is_empty() {
            let size = fs::metadata(&resolved)
                .map_err(|source| SessionError::Io {
                    path: resolved.clone(),
                    source,
                })?
                .len();
            if size > 0 {
                return Err(SessionError::InvalidSessionFile(resolved));
            }
            // Empty file: init header and rewrite.
            self.new_session(None)?;
            self.session_file = Some(resolved);
            self.rewrite_file()?;
            self.flushed = true;
            return Ok(());
        }

        let header_id = entries
            .iter()
            .find(|entry| entry.is_session_header())
            .and_then(|entry| match entry {
                FileEntry::Header(header) => header.id.clone(),
                FileEntry::RawHeader(raw) => {
                    raw.get("id").and_then(Value::as_str).map(str::to_owned)
                }
                FileEntry::Entry(_) => None,
            });
        self.session_id = header_id.unwrap_or_else(create_session_id);

        // Migration gate: v3+ files keep the directly parsed entries;
        // legacy files reload through the exact Value + migration path.
        let version = entries
            .iter()
            .find(|entry| entry.is_session_header())
            .map_or(1, |entry| match entry {
                FileEntry::Header(header) => header.version.unwrap_or(1),
                FileEntry::RawHeader(raw) => raw
                    .get("version")
                    .and_then(Value::as_u64)
                    .unwrap_or(1),
                FileEntry::Entry(_) => 1,
            });
        if version < CURRENT_SESSION_VERSION {
            let mut values =
                load_values_from_file(Path::new(&resolved)).map_err(|source| {
                    SessionError::Io {
                        path: resolved.clone(),
                        source,
                    }
                })?;
            let migrated = migrate_values_to_current(&mut values);
            self.file_entries = values
                .into_iter()
                .map(entries::file_entry_from_value)
                .collect();
            if migrated {
                self.rewrite_file()?;
            }
        } else {
            self.file_entries = entries;
        }
        self.build_index();
        self.flushed = true;
        Ok(())
    }

    /// Rebind the already-open session after its file was atomically moved.
    ///
    /// The caller owns the move and has already validated the file, so this must
    /// not re-read or rewrite it. The path is resolved (matching every other
    /// `session_file` assignment) and the session directory is retargeted to the
    /// new parent so later `new_session` and `create_branched_session` paths
    /// follow the moved file instead of the stale original directory.
    pub(crate) fn rebind_session_file_after_atomic_move(&mut self, session_file: &str) {
        let resolved = path_to_string(&resolve_path(session_file));
        self.session_dir = Path::new(&resolved)
            .parent()
            .map_or_else(|| ".".to_owned(), path_to_string);
        self.session_file = Some(resolved);
    }

    /// Start a new in-memory session (optionally with a deferred file path).
    ///
    /// Returns the session file path when persisting, else `None`.
    ///
    /// # Errors
    ///
    /// Returns a validation error when a custom session id is invalid.
    pub fn new_session(
        &mut self,
        options: Option<NewSessionOptions>,
    ) -> Result<Option<String>, SessionError> {
        if let Some(id) = options.as_ref().and_then(|opts| opts.id.as_ref()) {
            assert_valid_session_id(id)?;
        }
        let options = options.unwrap_or_default();
        self.session_id = options.id.unwrap_or_else(create_session_id);
        let timestamp = now_iso();
        let header = SessionHeader::new(
            self.session_id.clone(),
            timestamp.clone(),
            self.cwd.clone(),
            options.parent_session,
        );
        self.file_entries = vec![FileEntry::Header(header)];
        self.by_id.clear();
        self.labels.clear();
        self.leaf = Leaf::Null;
        self.flushed = false;

        if self.persist {
            let file_name = session_file_name(&timestamp, &self.session_id);
            self.session_file = Some(
                Path::new(&self.session_dir)
                    .join(file_name)
                    .to_string_lossy()
                    .into_owned(),
            );
        } else {
            self.session_file = None;
        }
        Ok(self.session_file.clone())
    }

    fn build_index(&mut self) {
        self.by_id.clear();
        self.labels.clear();
        self.leaf = Leaf::Null;
        for (idx, fe) in self.file_entries.iter().enumerate() {
            if fe.is_session_header() {
                continue;
            }
            let Some(entry) = fe.entry() else {
                continue;
            };
            if let Some(id) = entry.id() {
                self.by_id.insert(id.to_owned(), idx);
                self.leaf = Leaf::Id(id.to_owned());
            } else {
                self.leaf = Leaf::Tail;
            }
            if let Some((target, label)) = entry.label_fields() {
                match (target, label) {
                    (Some(t), Some(l)) if !l.is_empty() => {
                        let ts = entry.timestamp().unwrap_or("").to_owned();
                        self.labels.set(t.to_owned(), l.to_owned(), ts);
                    }
                    (Some(t), _) => {
                        self.labels.delete(t);
                    }
                    _ => {}
                }
            }
        }
    }

    fn rewrite_file(&self) -> Result<(), SessionError> {
        if !self.persist {
            return Ok(());
        }
        let Some(ref path) = self.session_file else {
            return Ok(());
        };
        atomic_replace_file(Path::new(path), |file| {
            for entry in &self.file_entries {
                let line = file_entry_to_line(entry)?;
                writeln!(file, "{line}").map_err(|source| SessionError::Io {
                    path: path.clone(),
                    source,
                })?;
            }
            Ok(())
        })
    }

    fn persist_entry_at(&mut self, idx: usize) -> Result<(), SessionError> {
        if !self.persist {
            return Ok(());
        }
        let Some(path) = self.session_file.clone() else {
            return Ok(());
        };

        let has_assistant = self
            .file_entries
            .iter()
            .filter_map(FileEntry::entry)
            .any(SessionEntry::is_assistant_message);

        if !has_assistant {
            if self.flushed
                && let Some(entry) = self.file_entries.get(idx).and_then(FileEntry::entry)
            {
                append_line(&path, entry)?;
            }
            // else: hold in memory (flushed stays false)
            return Ok(());
        }

        if self.flushed
            && let Some(entry) = self.file_entries.get(idx).and_then(FileEntry::entry)
        {
            append_line(&path, entry)?;
        } else {
            // Exclusive create (wx) + write ALL lines. Serialize before creating
            // the destination so JSON failures cannot leave a partial file.
            let mut contents = String::new();
            for fe in &self.file_entries {
                contents.push_str(&file_entry_to_line(fe)?);
                contents.push('\n');
            }
            let mut file = OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(&path)
                .map_err(|source| SessionError::Io {
                    path: path.clone(),
                    source,
                })?;
            if let Err(source) = file
                .write_all(contents.as_bytes())
                .and_then(|()| file.sync_all())
            {
                drop(file);
                let _ = fs::remove_file(&path);
                return Err(SessionError::Io {
                    path: path.clone(),
                    source,
                });
            }
            self.flushed = true;
        }
        Ok(())
    }

    fn append_entry(&mut self, entry: SessionEntry) -> Result<String, SessionError> {
        let id = entry.id().unwrap_or("").to_owned();
        let previous_leaf = self.leaf.clone();
        let previous_flushed = self.flushed;
        let previous_index = if id.is_empty() {
            None
        } else {
            self.by_id.insert(id.clone(), self.file_entries.len())
        };
        self.file_entries.push(FileEntry::Entry(entry));
        let idx = self.file_entries.len() - 1;
        self.leaf = if id.is_empty() {
            Leaf::Tail
        } else {
            Leaf::Id(id.clone())
        };
        if let Err(err) = self.persist_entry_at(idx) {
            self.file_entries.pop();
            if !id.is_empty() {
                match previous_index {
                    Some(index) => {
                        self.by_id.insert(id.clone(), index);
                    }
                    None => {
                        self.by_id.remove(&id);
                    }
                }
            }
            self.leaf = previous_leaf;
            self.flushed = previous_flushed;
            return Err(err);
        }
        Ok(id)
    }

    fn next_id(&self) -> String {
        generate_id(|c| self.by_id.contains_key(c))
    }

    // ----- accessors -----

    /// Whether this manager persists to disk.
    #[must_use]
    pub const fn is_persisted(&self) -> bool {
        self.persist
    }

    /// Resolved session working directory.
    #[must_use]
    pub fn get_cwd(&self) -> &str {
        &self.cwd
    }

    /// Session directory used for new/branch files.
    #[must_use]
    pub fn get_session_dir(&self) -> &str {
        &self.session_dir
    }

    /// True when `session_dir` equals the default encoded path for `cwd`.
    #[must_use]
    pub fn uses_default_session_dir(&self) -> bool {
        let default = default_session_dir_path(&self.cwd, &get_agent_dir());
        self.session_dir == default
    }

    /// Session UUID.
    #[must_use]
    pub fn get_session_id(&self) -> &str {
        &self.session_id
    }

    /// Session file path, if any.
    #[must_use]
    pub fn get_session_file(&self) -> Option<&str> {
        self.session_file.as_deref()
    }

    /// Current leaf entry id (`None` when null/tail).
    #[must_use]
    pub fn get_leaf_id(&self) -> Option<&str> {
        match &self.leaf {
            Leaf::Id(id) => Some(id.as_str()),
            Leaf::Null | Leaf::Tail => None,
        }
    }

    /// Current leaf entry.
    #[must_use]
    pub fn get_leaf_entry(&self) -> Option<&SessionEntry> {
        match &self.leaf {
            Leaf::Id(id) => self.get_entry(id),
            Leaf::Null | Leaf::Tail => None,
        }
    }

    /// Look up an entry by id.
    #[must_use]
    pub fn get_entry(&self, id: &str) -> Option<&SessionEntry> {
        self.by_id
            .get(id)
            .and_then(|&i| self.file_entries.get(i))
            .and_then(FileEntry::entry)
    }

    /// Direct children of `parent_id` (file order).
    #[must_use]
    pub fn get_children(&self, parent_id: &str) -> Vec<&SessionEntry> {
        self.file_entries
            .iter()
            .filter_map(FileEntry::entry)
            .filter(|e| e.parent_id() == Some(parent_id))
            .collect()
    }

    /// Resolved label for an entry id.
    #[must_use]
    pub fn get_label(&self, id: &str) -> Option<&str> {
        self.labels.get(id)
    }

    /// Session header, if typed parse succeeded.
    #[must_use]
    pub fn get_header(&self) -> Option<&SessionHeader> {
        self.file_entries.iter().find_map(FileEntry::header)
    }

    /// All non-header entries (file order).
    #[must_use]
    pub fn get_entries(&self) -> Vec<&SessionEntry> {
        self.file_entries
            .iter()
            .filter_map(FileEntry::entry)
            .collect()
    }

    /// Latest session display name (empty clears).
    #[must_use]
    pub fn get_session_name(&self) -> Option<String> {
        for entry in self.get_entries().into_iter().rev() {
            if let Some(name) = entry.session_info_name() {
                return name
                    .map(str::trim)
                    .filter(|s| !s.is_empty())
                    .map(str::to_owned);
            }
        }
        None
    }

    // ----- append -----

    /// Append a transcript message as child of the current leaf.
    ///
    /// # Errors
    ///
    /// Returns JSON serialization errors or IO errors when persisting.
    pub fn append_message(&mut self, message: &AgentMessage) -> Result<String, SessionError> {
        let id = self.next_id();
        let parent = self.leaf_parent();
        let timestamp = now_iso();
        let value = serde_json::json!({
            "type": "message",
            "id": id,
            "parentId": parent,
            "timestamp": timestamp,
            "message": message_to_value(message)?,
        });
        let entry: SessionEntry = serde_json::from_value(value)?;
        self.append_entry(entry)
    }

    /// Append a thinking-level change.
    ///
    /// # Errors
    ///
    /// Returns JSON or IO errors when encoding or persisting the entry.
    pub fn append_thinking_level_change(
        &mut self,
        thinking_level: &str,
    ) -> Result<String, SessionError> {
        let id = self.next_id();
        let value = serde_json::json!({
            "type": "thinking_level_change",
            "id": id,
            "parentId": self.leaf_parent(),
            "timestamp": now_iso(),
            "thinkingLevel": thinking_level,
        });
        let entry: SessionEntry = serde_json::from_value(value)?;
        self.append_entry(entry)
    }

    /// Append a model change.
    ///
    /// # Errors
    ///
    /// Returns JSON or IO errors when encoding or persisting the entry.
    pub fn append_model_change(
        &mut self,
        provider: &str,
        model_id: &str,
    ) -> Result<String, SessionError> {
        let id = self.next_id();
        let value = serde_json::json!({
            "type": "model_change",
            "id": id,
            "parentId": self.leaf_parent(),
            "timestamp": now_iso(),
            "provider": provider,
            "modelId": model_id,
        });
        let entry: SessionEntry = serde_json::from_value(value)?;
        self.append_entry(entry)
    }

    /// Append a compaction summary.
    ///
    /// # Errors
    ///
    /// Returns JSON or IO errors when encoding or persisting the entry.
    pub fn append_compaction(
        &mut self,
        summary: &str,
        first_kept_entry_id: &str,
        tokens_before: i64,
        details: Option<Value>,
        from_hook: Option<bool>,
        usage: Option<pi_ai::Usage>,
    ) -> Result<String, SessionError> {
        let id = self.next_id();
        let mut value = serde_json::json!({
            "type": "compaction",
            "id": id,
            "parentId": self.leaf_parent(),
            "timestamp": now_iso(),
            "summary": summary,
            "firstKeptEntryId": first_kept_entry_id,
            "tokensBefore": tokens_before,
        });
        if let Some(d) = details
            && let Some(obj) = value.as_object_mut()
        {
            obj.insert("details".to_owned(), d);
        }
        if let Some(fh) = from_hook
            && let Some(obj) = value.as_object_mut()
        {
            obj.insert("fromHook".to_owned(), Value::Bool(fh));
        }
        if let Some(u) = usage
            && let Some(obj) = value.as_object_mut()
        {
            obj.insert("usage".to_owned(), serde_json::to_value(u)?);
        }
        let entry: SessionEntry = serde_json::from_value(value)?;
        self.append_entry(entry)
    }

    /// Append an extension custom state entry (not in LLM context).
    ///
    /// # Errors
    ///
    /// Returns JSON or IO errors when encoding or persisting the entry.
    pub fn append_custom_entry(
        &mut self,
        custom_type: &str,
        data: Option<Value>,
    ) -> Result<String, SessionError> {
        let id = self.next_id();
        let mut value = serde_json::json!({
            "type": "custom",
            "customType": custom_type,
            "id": id,
            "parentId": self.leaf_parent(),
            "timestamp": now_iso(),
        });
        if let Some(d) = data
            && let Some(obj) = value.as_object_mut()
        {
            obj.insert("data".to_owned(), d);
        }
        let entry: SessionEntry = serde_json::from_value(value)?;
        self.append_entry(entry)
    }

    /// Append a session display-name entry (sanitized).
    ///
    /// # Errors
    ///
    /// Returns JSON or IO errors when encoding or persisting the entry.
    pub fn append_session_info(&mut self, name: &str) -> Result<String, SessionError> {
        // TS: name.replace(/[\r\n]+/g, " ").trim()
        let sanitized = {
            let mut result = String::with_capacity(name.len());
            let mut chars = name.chars().peekable();
            while let Some(c) = chars.next() {
                if c == '\r' || c == '\n' {
                    while matches!(chars.peek(), Some('\r' | '\n')) {
                        chars.next();
                    }
                    result.push(' ');
                } else {
                    result.push(c);
                }
            }
            result.trim().to_owned()
        };
        let id = self.next_id();
        let value = serde_json::json!({
            "type": "session_info",
            "id": id,
            "parentId": self.leaf_parent(),
            "timestamp": now_iso(),
            "name": sanitized,
        });
        let entry: SessionEntry = serde_json::from_value(value)?;
        self.append_entry(entry)
    }

    /// Append an extension custom message (participates in LLM context).
    ///
    /// # Errors
    ///
    /// Returns JSON or IO errors when encoding or persisting the entry.
    pub fn append_custom_message_entry(
        &mut self,
        custom_type: &str,
        content: &CustomMessageContent,
        display: bool,
        details: Option<Value>,
    ) -> Result<String, SessionError> {
        let id = self.next_id();
        let mut value = serde_json::json!({
            "type": "custom_message",
            "customType": custom_type,
            "content": content,
            "display": display,
            "id": id,
            "parentId": self.leaf_parent(),
            "timestamp": now_iso(),
        });
        if let Some(d) = details
            && let Some(obj) = value.as_object_mut()
        {
            obj.insert("details".to_owned(), d);
        }
        let entry: SessionEntry = serde_json::from_value(value)?;
        self.append_entry(entry)
    }

    /// Set or clear a label on an entry. Empty/None clears.
    ///
    /// # Errors
    ///
    /// Returns not-found when `target_id` is missing, or JSON/IO errors on persist.
    pub fn append_label_change(
        &mut self,
        target_id: &str,
        label: Option<&str>,
    ) -> Result<String, SessionError> {
        if !self.by_id.contains_key(target_id) {
            return Err(SessionError::EntryNotFound(target_id.to_owned()));
        }
        let id = self.next_id();
        let timestamp = now_iso();
        let mut value = serde_json::json!({
            "type": "label",
            "id": id,
            "parentId": self.leaf_parent(),
            "timestamp": timestamp,
            "targetId": target_id,
        });
        if let Some(l) = label
            && let Some(obj) = value.as_object_mut()
        {
            obj.insert("label".to_owned(), Value::String(l.to_owned()));
        }
        let entry: SessionEntry = serde_json::from_value(value)?;
        let result_id = self.append_entry(entry)?;
        match label {
            Some(l) if !l.is_empty() => {
                self.labels
                    .set(target_id.to_owned(), l.to_owned(), timestamp);
            }
            _ => {
                self.labels.delete(target_id);
            }
        }
        Ok(result_id)
    }

    // ----- tree -----

    /// Walk from `from_id` (or current leaf) to root; root→leaf order.
    ///
    /// Unlike context path construction, a missing id returns an empty path
    /// (no last-entry fallback).
    #[must_use]
    pub fn get_branch(&self, from_id: Option<&str>) -> Vec<&SessionEntry> {
        let start = from_id.or(match &self.leaf {
            Leaf::Id(id) => Some(id.as_str()),
            Leaf::Null | Leaf::Tail => None,
        });
        let Some(start_id) = start.filter(|s| !s.is_empty()) else {
            return Vec::new();
        };
        let mut path = Vec::new();
        let mut current = self.get_entry(start_id);
        while let Some(entry) = current {
            path.push(entry);
            current = entry.parent_id().and_then(|pid| self.get_entry(pid));
        }
        path.reverse();
        path
    }

    /// Compaction-aware context entries for the current leaf.
    #[must_use]
    pub fn build_context_entries(&self) -> Vec<&SessionEntry> {
        let entries = self.get_entries();
        let leaf = self.leaf_ref();
        context::build_context_entries(&entries, leaf)
    }

    /// Full session context for the LLM (current leaf).
    ///
    /// # Errors
    ///
    /// Returns message conversion errors when projecting entries to context messages.
    pub fn build_session_context(&self) -> Result<SessionContext, MessageConversionError> {
        let entries = self.get_entries();
        let leaf = self.leaf_ref();
        context::build_session_context(&entries, leaf)
    }

    /// Session as a tree (orphans become roots; children sorted by timestamp).
    #[must_use]
    pub fn get_tree(&self) -> Vec<SessionTreeNode> {
        let entries = self.get_entries();
        let mut node_map: HashMap<String, SessionTreeNode> = HashMap::new();
        let mut order: Vec<String> = Vec::new();

        for entry in &entries {
            let Some(id) = entry.id() else {
                continue;
            };
            if !node_map.contains_key(id) {
                order.push(id.to_owned());
            }
            let label = self.labels.get(id).map(str::to_owned);
            let label_timestamp = self.labels.get_timestamp(id).map(str::to_owned);
            node_map.insert(
                id.to_owned(),
                SessionTreeNode {
                    entry: (*entry).clone(),
                    children: Vec::new(),
                    label,
                    label_timestamp,
                },
            );
        }

        let mut roots: Vec<String> = Vec::new();
        let mut child_links: Vec<(String, String)> = Vec::new(); // (parent, child)

        for entry in &entries {
            let Some(id) = entry.id() else {
                continue;
            };
            match entry.parent_id() {
                Some(pid) if pid != id && node_map.contains_key(pid) => {
                    child_links.push((pid.to_owned(), id.to_owned()));
                }
                None | Some(_) => roots.push(id.to_owned()),
            }
        }

        let mut children_of: HashMap<String, Vec<String>> = HashMap::new();
        for (parent, child) in child_links {
            children_of.entry(parent).or_default().push(child);
        }

        // Deduplicate roots (dup ids).
        let mut seen = HashSet::new();
        let mut root_nodes = Vec::new();
        for id in roots {
            if seen.insert(id.clone())
                && let Some(node) = assemble_tree_node(&id, &node_map, &children_of)
            {
                root_nodes.push(node);
            }
        }
        // Sort root children already done per-node; roots themselves unsorted in TS
        // (file order of first encounter). Keep as-is.
        let _ = order;
        root_nodes
    }

    // ----- branching -----

    /// Move the leaf pointer to an existing entry.
    ///
    /// # Errors
    ///
    /// Returns not-found when `branch_from_id` is missing.
    pub fn branch(&mut self, branch_from_id: &str) -> Result<(), SessionError> {
        if !self.by_id.contains_key(branch_from_id) {
            return Err(SessionError::EntryNotFound(branch_from_id.to_owned()));
        }
        self.leaf = Leaf::Id(branch_from_id.to_owned());
        Ok(())
    }

    /// Reset the leaf pointer to null (next append is a new root).
    pub fn reset_leaf(&mut self) {
        self.leaf = Leaf::Null;
    }

    /// Branch from an entry (or null root) and append a `branch_summary` child.
    ///
    /// # Errors
    ///
    /// Returns not-found when a branch id is missing, or JSON/IO errors on persist.
    pub fn branch_with_summary(
        &mut self,
        branch_from_id: Option<&str>,
        summary: &str,
        details: Option<Value>,
        from_hook: Option<bool>,
        usage: Option<pi_ai::Usage>,
    ) -> Result<String, SessionError> {
        if let Some(id) = branch_from_id {
            if !self.by_id.contains_key(id) {
                return Err(SessionError::EntryNotFound(id.to_owned()));
            }
            self.leaf = Leaf::Id(id.to_owned());
        } else {
            self.leaf = Leaf::Null;
        }
        let id = self.next_id();
        let from_id = branch_from_id.unwrap_or("root");
        let mut value = serde_json::json!({
            "type": "branch_summary",
            "id": id,
            "parentId": branch_from_id,
            "timestamp": now_iso(),
            "fromId": from_id,
            "summary": summary,
        });
        if let Some(d) = details
            && let Some(obj) = value.as_object_mut()
        {
            obj.insert("details".to_owned(), d);
        }
        if let Some(fh) = from_hook
            && let Some(obj) = value.as_object_mut()
        {
            obj.insert("fromHook".to_owned(), Value::Bool(fh));
        }
        if let Some(u) = usage
            && let Some(obj) = value.as_object_mut()
        {
            obj.insert("usage".to_owned(), serde_json::to_value(u)?);
        }
        let entry: SessionEntry = serde_json::from_value(value)?;
        self.append_entry(entry)
    }

    /// Create a new session containing only the path to `leaf_id`.
    ///
    /// Strips label entries, rechains parent ids, re-appends resolved labels
    /// with new ids but preserved timestamps. Returns the new file path when
    /// persisting (or `None` in-memory).
    ///
    /// # Errors
    ///
    /// Returns not-found when `leaf_id` is missing, or JSON/IO errors while writing.
    pub fn create_branched_session(
        &mut self,
        leaf_id: &str,
    ) -> Result<Option<String>, SessionError> {
        let previous_session_file = self.session_file.clone();
        let path: Vec<SessionEntry> = self
            .get_branch(Some(leaf_id))
            .into_iter()
            .cloned()
            .collect();
        if path.is_empty() {
            return Err(SessionError::EntryNotFound(leaf_id.to_owned()));
        }

        let mut path_without_labels: Vec<SessionEntry> = Vec::new();
        let mut path_parent: Option<String> = None;
        for entry in path {
            if entry.discriminant() == "label" {
                continue;
            }
            let mut cloned = entry;
            cloned.set_parent_id(path_parent.clone());
            path_parent = cloned.id().map(str::to_owned);
            path_without_labels.push(cloned);
        }

        let new_session_id = create_session_id();
        let timestamp = now_iso();
        let file_name = session_file_name(&timestamp, &new_session_id);
        let new_session_file = Path::new(&self.session_dir)
            .join(&file_name)
            .to_string_lossy()
            .into_owned();

        let header = SessionHeader::new(
            new_session_id.clone(),
            timestamp,
            self.cwd.clone(),
            if self.persist {
                previous_session_file
            } else {
                None
            },
        );

        let path_ids: HashSet<String> = path_without_labels
            .iter()
            .filter_map(|e| e.id().map(str::to_owned))
            .collect();

        let labels_to_write: Vec<(String, String, String)> = self
            .labels
            .iter()
            .filter(|(t, _, _)| path_ids.contains(*t))
            .map(|(t, l, ts)| (t.to_owned(), l.to_owned(), ts.to_owned()))
            .collect();

        let mut collision: HashSet<String> = path_ids;
        let mut parent = path_without_labels
            .last()
            .and_then(|e| e.id().map(str::to_owned));
        let mut label_entries: Vec<SessionEntry> = Vec::new();
        for (target, label, ts) in labels_to_write {
            let lid = generate_id(|c| collision.contains(c));
            collision.insert(lid.clone());
            let value = serde_json::json!({
                "type": "label",
                "id": lid,
                "parentId": parent,
                "timestamp": ts,
                "targetId": target,
                "label": label,
            });
            let entry: SessionEntry = serde_json::from_value(value)?;
            parent = entry.id().map(str::to_owned);
            label_entries.push(entry);
        }

        let mut file_entries = vec![FileEntry::Header(header)];
        file_entries.extend(path_without_labels.into_iter().map(FileEntry::Entry));
        file_entries.extend(label_entries.into_iter().map(FileEntry::Entry));

        self.file_entries = file_entries;
        self.session_id = new_session_id;
        self.build_index();

        if self.persist {
            self.session_file = Some(new_session_file.clone());
            let has_assistant = self
                .file_entries
                .iter()
                .filter_map(FileEntry::entry)
                .any(SessionEntry::is_assistant_message);
            if has_assistant {
                self.rewrite_file()?;
                self.flushed = true;
            } else {
                self.flushed = false;
            }
            Ok(Some(new_session_file))
        } else {
            // In-memory: session_file and flushed untouched.
            Ok(None)
        }
    }

    // ----- static factories -----

    /// Create a new persisted session.
    ///
    /// # Errors
    ///
    /// Returns validation errors for custom ids, or IO errors creating the session dir.
    pub fn create(
        cwd: &str,
        session_dir: Option<&str>,
        options: Option<NewSessionOptions>,
    ) -> Result<Self, SessionError> {
        let dir = match session_dir {
            Some(d) => path_to_string(&normalize_path(d, PathInputOptions::new())),
            None => default_session_dir(cwd, &get_agent_dir())?,
        };
        Self::construct(cwd, &dir, None, true, options)
    }

    /// Open a specific session file.
    ///
    /// # Errors
    ///
    /// Returns IO/invalid-session errors while loading the file, or validation/IO
    /// errors while constructing the manager.
    pub fn open(
        path: &str,
        session_dir: Option<&str>,
        cwd_override: Option<&str>,
    ) -> Result<Self, SessionError> {
        let resolved = path_to_string(&resolve_path(path));
        // Single-pass open: the one parse below feeds both the header-cwd
        // probe and the manager state (the file was previously parsed twice —
        // a full `load_entries_from_file` probe, then `set_session_file`'s
        // reload). Loader and error semantics match `set_session_file`:
        // missing file → new session targeting the path; read failure → `Io`.
        let exists = path_exists(Path::new(&resolved));
        let entries = if exists {
            load_file_entries_from_file(Path::new(&resolved)).map_err(|source| {
                SessionError::Io {
                    path: resolved.clone(),
                    source,
                }
            })?
        } else {
            Vec::new()
        };
        let header_cwd = entries
            .iter()
            .find_map(FileEntry::header)
            .and_then(|h| h.cwd.clone());
        let cwd = cwd_override.map_or_else(
            || {
                header_cwd.unwrap_or_else(|| {
                    std::env::current_dir()
                        .map_or_else(|_| ".".to_owned(), |p| p.to_string_lossy().into_owned())
                })
            },
            str::to_owned,
        );
        let dir = match session_dir {
            Some(d) => path_to_string(&normalize_path(d, PathInputOptions::new())),
            None => Path::new(&resolved)
                .parent()
                .map_or_else(|| ".".to_owned(), |p| p.to_string_lossy().into_owned()),
        };
        let mut sm = Self::construct_empty(&cwd, &dir, true)?;
        sm.session_file = Some(resolved.clone());
        if exists {
            sm.apply_session_file_entries(resolved, entries)?;
        } else {
            sm.new_session(None)?;
            sm.session_file = Some(resolved);
        }
        Ok(sm)
    }

    /// Continue the most recent session, or create new if none.
    ///
    /// # Errors
    ///
    /// Returns IO errors discovering sessions or constructing a new manager.
    pub fn continue_recent(cwd: &str, session_dir: Option<&str>) -> Result<Self, SessionError> {
        let dir = match session_dir {
            Some(d) => path_to_string(&normalize_path(d, PathInputOptions::new())),
            None => default_session_dir(cwd, &get_agent_dir())?,
        };
        let filter_cwd =
            session_dir.is_some() && dir != default_session_dir_path(cwd, &get_agent_dir());
        let most_recent =
            find_most_recent_session(Path::new(&dir), if filter_cwd { Some(cwd) } else { None });
        match most_recent {
            Some(f) => Self::construct(
                cwd,
                &dir,
                Some(f.to_string_lossy().into_owned()),
                true,
                None,
            ),
            None => Self::construct(cwd, &dir, None, true, None),
        }
    }

    /// Create an in-memory session (no file persistence).
    ///
    /// # Errors
    ///
    /// Returns a validation error when a custom session id is invalid.
    pub fn in_memory(
        cwd: Option<&str>,
        options: Option<NewSessionOptions>,
    ) -> Result<Self, SessionError> {
        let cwd = cwd.map_or_else(
            || {
                std::env::current_dir()
                    .map_or_else(|_| ".".to_owned(), |p| p.to_string_lossy().into_owned())
            },
            str::to_owned,
        );
        Self::construct(&cwd, "", None, false, options)
    }

    /// Fork a session into a new file under `target_cwd`.
    ///
    /// Copies **all** non-header source entries as-is (IDs preserved) under a
    /// new v3 header with `parentSession = source path`.
    ///
    /// # Errors
    ///
    /// Returns fork-source/validation errors, or IO/JSON errors while writing the fork.
    pub fn fork_from(
        source_path: &str,
        target_cwd: &str,
        session_dir: Option<&str>,
        options: Option<NewSessionOptions>,
    ) -> Result<Self, SessionError> {
        let resolved_source = path_to_string(&resolve_path(source_path));
        let resolved_target = path_to_string(&resolve_path(target_cwd));
        let values = load_values_from_file(Path::new(&resolved_source)).map_err(|source| {
            SessionError::Io {
                path: resolved_source.clone(),
                source,
            }
        })?;
        if values.is_empty() {
            return Err(SessionError::ForkSourceEmpty(resolved_source));
        }
        if !values
            .iter()
            .any(|v| v.get("type").and_then(Value::as_str) == Some("session"))
        {
            return Err(SessionError::ForkSourceNoHeader(resolved_source));
        }

        let dir = match session_dir {
            Some(d) => path_to_string(&normalize_path(d, PathInputOptions::new())),
            None => default_session_dir(&resolved_target, &get_agent_dir())?,
        };
        if !path_exists(Path::new(&dir)) {
            fs::create_dir_all(&dir).map_err(|source| SessionError::Io {
                path: dir.clone(),
                source,
            })?;
        }

        if let Some(id) = options.as_ref().and_then(|opts| opts.id.as_ref()) {
            assert_valid_session_id(id)?;
        }
        let options = options.unwrap_or_default();
        let new_session_id = options.id.unwrap_or_else(create_session_id);
        let timestamp = now_iso();
        let file_name = session_file_name(&timestamp, &new_session_id);
        let new_session_file = Path::new(&dir)
            .join(file_name)
            .to_string_lossy()
            .into_owned();

        let header = SessionHeader::new(
            new_session_id,
            timestamp,
            resolved_target.clone(),
            Some(resolved_source),
        );

        let mut file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&new_session_file)
            .map_err(|source| SessionError::Io {
                path: new_session_file.clone(),
                source,
            })?;
        writeln!(file, "{}", serde_json::to_string(&header)?).map_err(|source| {
            SessionError::Io {
                path: new_session_file.clone(),
                source,
            }
        })?;
        for value in &values {
            if value.get("type").and_then(Value::as_str) == Some("session") {
                continue;
            }
            writeln!(file, "{}", serde_json::to_string(value)?).map_err(|source| {
                SessionError::Io {
                    path: new_session_file.clone(),
                    source,
                }
            })?;
        }
        drop(file);

        Self::construct(&resolved_target, &dir, Some(new_session_file), true, None)
    }

    /// List sessions for a cwd.
    ///
    /// # Errors
    ///
    /// Returns IO errors while enumerating or reading session files.
    pub async fn list(
        cwd: &str,
        session_dir: Option<&str>,
        on_progress: Option<SessionListProgress<'_>>,
    ) -> Result<Vec<SessionInfo>, SessionError> {
        let dir = match session_dir {
            Some(d) => path_to_string(&normalize_path(d, PathInputOptions::new())),
            None => default_session_dir(cwd, &get_agent_dir())?,
        };
        let filter_cwd =
            session_dir.is_some() && dir != default_session_dir_path(cwd, &get_agent_dir());
        Ok(list_sessions_for_cwd(cwd, Path::new(&dir), filter_cwd, on_progress).await)
    }

    /// List all sessions under the agent sessions root (or a custom dir).
    pub async fn list_all(
        session_dir: Option<&str>,
        on_progress: Option<SessionListProgress<'_>>,
    ) -> Vec<SessionInfo> {
        let custom =
            session_dir.map(|d| path_to_string(&normalize_path(d, PathInputOptions::new())));
        let root = get_sessions_dir();
        list_all_sessions(&root, custom.as_deref().map(Path::new), on_progress).await
    }

    // ----- helpers -----

    fn leaf_parent(&self) -> Option<String> {
        match &self.leaf {
            Leaf::Id(id) => Some(id.clone()),
            Leaf::Null | Leaf::Tail => None,
        }
    }

    fn leaf_ref(&self) -> LeafRef<'_> {
        match &self.leaf {
            Leaf::Null => LeafRef::Null,
            Leaf::Id(id) => LeafRef::Id(id.as_str()),
            Leaf::Tail => LeafRef::Last,
        }
    }
}

impl SessionCwdSource for SessionManager {
    fn get_cwd(&self) -> &str {
        &self.cwd
    }
    fn get_session_file(&self) -> Option<&str> {
        self.session_file.as_deref()
    }
}

// ---------------------------------------------------------------------------
// Path encoding / helpers
// ---------------------------------------------------------------------------

/// Encode a resolved cwd into a safe session-directory name segment.
///
/// Algorithm: strip one leading `/` or `\`, replace `/`, `\`, `:` with `-`,
/// wrap in `--…--`.
#[must_use]
pub fn encode_cwd_for_session_dir(resolved_cwd: &str) -> String {
    let stripped = resolved_cwd
        .strip_prefix('/')
        .or_else(|| resolved_cwd.strip_prefix('\\'))
        .unwrap_or(resolved_cwd);
    let safe: String = stripped
        .chars()
        .map(|c| {
            if matches!(c, '/' | '\\' | ':') {
                '-'
            } else {
                c
            }
        })
        .collect();
    format!("--{safe}--")
}

/// Default session directory path for a cwd (does not create).
#[must_use]
pub fn default_session_dir_path(cwd: &str, agent_dir: &Path) -> String {
    let resolved_cwd = path_to_string(&resolve_path(cwd));
    let resolved_agent = path_to_string(&resolve_path(agent_dir.to_string_lossy()));
    let name = encode_cwd_for_session_dir(&resolved_cwd);
    Path::new(&resolved_agent)
        .join("sessions")
        .join(name)
        .to_string_lossy()
        .into_owned()
}

/// Default session directory for a cwd (creates if missing).
///
/// # Errors
///
/// Returns IO errors when creating the directory.
pub fn default_session_dir(cwd: &str, agent_dir: &Path) -> Result<String, SessionError> {
    let dir = default_session_dir_path(cwd, agent_dir);
    if !path_exists(Path::new(&dir)) {
        fs::create_dir_all(&dir).map_err(|source| SessionError::Io {
            path: dir.clone(),
            source,
        })?;
    }
    Ok(dir)
}

fn session_file_name(timestamp_iso: &str, session_id: &str) -> String {
    let file_ts: String = timestamp_iso
        .chars()
        .map(|c| if c == ':' || c == '.' { '-' } else { c })
        .collect();
    format!("{file_ts}_{session_id}.jsonl")
}

fn path_to_string(path: &Path) -> String {
    path.to_string_lossy().into_owned()
}

fn append_line(path: &str, entry: &SessionEntry) -> Result<(), SessionError> {
    let line = session_entry_to_line(entry)?;
    let mut record = String::with_capacity(line.len() + 1);
    record.push_str(&line);
    record.push('\n');
    let mut file = OpenOptions::new()
        .append(true)
        .create(true)
        .open(path)
        .map_err(|source| SessionError::Io {
            path: path.to_owned(),
            source,
        })?;
    file.write_all(record.as_bytes())
        .map_err(|source| SessionError::Io {
            path: path.to_owned(),
            source,
        })
}

fn message_to_value(message: &AgentMessage) -> Result<Value, SessionError> {
    Ok(serde_json::to_value(message)?)
}

static TEMP_FILE_COUNTER: AtomicU64 = AtomicU64::new(0);

fn atomic_replace_file(
    path: &Path,
    write_contents: impl FnOnce(&mut File) -> Result<(), SessionError>,
) -> Result<(), SessionError> {
    let path_text = path.to_string_lossy().into_owned();
    let parent = path.parent().unwrap_or_else(|| Path::new("."));
    let file_name = path
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("session");
    let mut temp_path = PathBuf::new();
    let mut temp_file = None;
    for _ in 0..100 {
        let suffix = TEMP_FILE_COUNTER.fetch_add(1, Ordering::Relaxed);
        temp_path = parent.join(format!(".{file_name}.tmp-{}-{suffix}", std::process::id()));
        match OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&temp_path)
        {
            Ok(file) => {
                temp_file = Some(file);
                break;
            }
            Err(err) if err.kind() == io::ErrorKind::AlreadyExists => {}
            Err(source) => {
                return Err(SessionError::Io {
                    path: temp_path.to_string_lossy().into_owned(),
                    source,
                });
            }
        }
    }
    let Some(mut file) = temp_file else {
        return Err(SessionError::Io {
            path: path_text,
            source: io::Error::new(
                io::ErrorKind::AlreadyExists,
                "unable to allocate temporary session file",
            ),
        });
    };

    let result = write_contents(&mut file)
        .and_then(|()| {
            file.flush().map_err(|source| SessionError::Io {
                path: temp_path.to_string_lossy().into_owned(),
                source,
            })
        })
        .and_then(|()| {
            file.sync_all().map_err(|source| SessionError::Io {
                path: temp_path.to_string_lossy().into_owned(),
                source,
            })
        });
    drop(file);
    if let Err(err) = result {
        let _ = fs::remove_file(&temp_path);
        return Err(err);
    }

    if let Err(source) = fs::rename(&temp_path, path) {
        let _ = fs::remove_file(&temp_path);
        return Err(SessionError::Io {
            path: path_text,
            source,
        });
    }
    sync_parent_directory(parent).map_err(|source| SessionError::Io {
        path: parent.to_string_lossy().into_owned(),
        source,
    })
}

#[cfg(unix)]
fn sync_parent_directory(parent: &Path) -> io::Result<()> {
    File::open(parent)?.sync_all()
}

#[cfg(not(unix))]
fn sync_parent_directory(_parent: &Path) -> io::Result<()> {
    Ok(())
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use pi_ai::{Message, TextContent, Usage, UsageCost, UserMessage, UserMessageContent};
    use serde_json::json;
    use tempfile::tempdir;

    type TestResult = Result<(), Box<dyn std::error::Error>>;

    #[test]
    fn atomic_replace_failure_preserves_live_file() -> TestResult {
        let dir = tempdir()?;
        let file = dir.path().join("atomic.jsonl");
        fs::write(&file, "original\n")?;

        let result = atomic_replace_file(&file, |temp| {
            temp.write_all(b"partial replacement\n")
                .map_err(|source| SessionError::Io {
                    path: file.to_string_lossy().into_owned(),
                    source,
                })?;
            Err(SessionError::Io {
                path: file.to_string_lossy().into_owned(),
                source: io::Error::other("injected rewrite failure"),
            })
        });

        assert!(result.is_err());
        assert_eq!(fs::read_to_string(&file)?, "original\n");
        let leftovers: Vec<_> = fs::read_dir(dir.path())?
            .flatten()
            .filter(|entry| entry.file_name().to_string_lossy().contains(".tmp-"))
            .collect();
        assert!(leftovers.is_empty(), "failed rewrite must remove temp file");
        Ok(())
    }

    fn path_str(path: &Path) -> Result<&str, Box<dyn std::error::Error>> {
        path.to_str()
            .ok_or_else(|| "path is not valid UTF-8".into())
    }

    fn user_agent(text: &str, ts: i64) -> AgentMessage {
        AgentMessage::Llm(Box::new(Message::User(UserMessage::new(
            UserMessageContent::Text(text.to_owned()),
            ts,
        ))))
    }

    fn assistant_agent(text: &str, ts: i64) -> AgentMessage {
        let mut msg =
            pi_ai::AssistantMessage::new("anthropic-messages", "anthropic", "claude-test", ts);
        msg.content = vec![pi_ai::AssistantContent::Text(TextContent::new(text))];
        msg.usage = pi_ai::Usage {
            input: 1,
            output: 1,
            cache_read: 0,
            cache_write: 0,
            total_tokens: 2,
            cost: pi_ai::UsageCost::default(),
            ..Default::default()
        };
        AgentMessage::Llm(Box::new(Message::Assistant(msg)))
    }

    #[test]
    fn deferred_write_until_first_assistant() -> TestResult {
        let dir = tempdir()?;
        let mut sm =
            SessionManager::create(path_str(dir.path())?, Some(path_str(dir.path())?), None)?;
        let file = sm.get_session_file().ok_or("file")?.to_owned();
        assert!(!path_exists(Path::new(&file)), "file must not exist yet");

        sm.append_message(&user_agent("hello", 1))?;
        assert!(!path_exists(Path::new(&file)), "still deferred after user");

        sm.append_message(&assistant_agent("hi", 2))?;
        assert!(path_exists(Path::new(&file)), "created on assistant");

        let content = fs::read_to_string(&file)?;
        let lines: Vec<_> = content.lines().filter(|l| !l.is_empty()).collect();
        assert_eq!(lines.len(), 3); // header + user + assistant
        let header: Value = serde_json::from_str(lines[0])?;
        assert_eq!(header["type"], "session");
        assert_eq!(header["version"], 3);
        Ok(())
    }

    #[test]
    fn atomic_move_path_rebind_persists_to_final_file() -> TestResult {
        let dir = tempdir()?;
        let mut source =
            SessionManager::create(path_str(dir.path())?, Some(path_str(dir.path())?), None)?;
        let source_file = source.get_session_file().ok_or("source file")?.to_owned();
        source.append_message(&user_agent("before move", 1))?;
        source.append_message(&assistant_agent("persist source", 2))?;

        let staged = dir.path().join("import.tmp");
        let final_file = dir.path().join("imported.jsonl");
        fs::rename(&source_file, &staged)?;
        let mut reopened =
            SessionManager::open(path_str(&staged)?, Some(path_str(dir.path())?), None)?;
        fs::rename(&staged, &final_file)?;
        reopened.rebind_session_file_after_atomic_move(path_str(&final_file)?);

        reopened.append_message(&user_agent("after move", 3))?;

        assert_eq!(reopened.get_session_file(), Some(path_str(&final_file)?));
        assert!(!staged.exists());
        assert!(fs::read_to_string(&final_file)?.contains("after move"));
        Ok(())
    }

    #[test]
    fn atomic_move_rebind_retargets_session_dir() -> TestResult {
        let dir = tempdir()?;
        let other = tempdir()?;
        let mut source =
            SessionManager::create(path_str(dir.path())?, Some(path_str(dir.path())?), None)?;
        source.append_message(&user_agent("before move", 1))?;
        source.append_message(&assistant_agent("persist source", 2))?;
        let source_file = source.get_session_file().ok_or("source file")?.to_owned();

        // Move the file into a *different* directory than the original
        // session_dir, then rebind. A later branch must land next to the
        // moved file, not the stale original directory.
        let final_file = other.path().join("imported.jsonl");
        fs::rename(&source_file, &final_file)?;
        let mut reopened =
            SessionManager::open(path_str(&final_file)?, Some(path_str(other.path())?), None)?;
        reopened.rebind_session_file_after_atomic_move(path_str(&final_file)?);

        assert_eq!(reopened.get_session_file(), Some(path_str(&final_file)?));
        assert_eq!(reopened.get_session_dir(), path_str(other.path())?);

        let leaf = reopened.get_leaf_id().ok_or("leaf")?.to_owned();
        let branched = reopened
            .create_branched_session(&leaf)?
            .ok_or("branched file")?;
        assert!(
            Path::new(&branched).starts_with(other.path()),
            "branch {branched} must follow the retargeted dir, not the original"
        );
        assert!(Path::new(&branched).exists());
        Ok(())
    }

    #[test]
    fn append_prefix_stability() -> TestResult {
        let dir = tempdir()?;
        let file = dir.path().join("stable.jsonl");
        let original = concat!(
            r#"{"type":"session","version":3,"id":"sess-1","timestamp":"2025-01-01T00:00:00.000Z","cwd":"/tmp"}"#,
            "\n",
            r#"{"type":"message","id":"aaaaaaaa","parentId":null,"timestamp":"2025-01-01T00:00:01.000Z","message":{"role":"user","content":"hi","timestamp":1}}"#,
            "\n",
            r#"{"type":"message","id":"bbbbbbbb","parentId":"aaaaaaaa","timestamp":"2025-01-01T00:00:02.000Z","message":{"role":"assistant","content":[{"type":"text","text":"yo"}],"api":"test","provider":"test","model":"test","usage":{"input":1,"output":1,"cacheRead":0,"cacheWrite":0,"totalTokens":2,"cost":{"input":0,"output":0,"cacheRead":0,"cacheWrite":0,"total":0}},"stopReason":"stop","timestamp":2}}"#,
            "\n",
        );
        fs::write(&file, original)?;

        let mut sm = SessionManager::open(path_str(&file)?, Some(path_str(dir.path())?), None)?;
        sm.append_message(&user_agent("next", 3))?;

        let after = fs::read(&file)?;
        assert!(
            after.starts_with(original.as_bytes()),
            "original lines must be byte-stable on append"
        );
        Ok(())
    }

    #[test]
    fn failed_append_does_not_advance_tree_and_can_retry() -> TestResult {
        let dir = tempdir()?;
        let file = dir.path().join("retry.jsonl");
        let original = concat!(
            r#"{"type":"session","version":3,"id":"sess-1","timestamp":"2025-01-01T00:00:00.000Z","cwd":"/tmp"}"#,
            "\n",
            r#"{"type":"message","id":"aaaaaaaa","parentId":null,"timestamp":"2025-01-01T00:00:01.000Z","message":{"role":"user","content":"hi","timestamp":1}}"#,
            "\n",
            r#"{"type":"message","id":"bbbbbbbb","parentId":"aaaaaaaa","timestamp":"2025-01-01T00:00:02.000Z","message":{"role":"assistant","content":[{"type":"text","text":"yo"}],"api":"test","provider":"test","model":"test","usage":{"input":1,"output":1,"cacheRead":0,"cacheWrite":0,"totalTokens":2,"cost":{"input":0,"output":0,"cacheRead":0,"cacheWrite":0,"total":0}},"stopReason":"stop","timestamp":2}}"#,
            "\n",
        );
        fs::write(&file, original)?;
        let mut sm = SessionManager::open(path_str(&file)?, Some(path_str(dir.path())?), None)?;
        let before_count = sm.get_entries().len();
        let before_leaf = sm.get_leaf_id().map(str::to_owned);

        let backup = dir.path().join("retry.backup");
        fs::rename(&file, &backup)?;
        fs::create_dir(&file)?;
        let result = sm.append_message(&user_agent("retry me", 3));
        assert!(matches!(result, Err(SessionError::Io { .. })));
        assert_eq!(sm.get_entries().len(), before_count);
        assert_eq!(sm.get_leaf_id(), before_leaf.as_deref());

        fs::remove_dir(&file)?;
        fs::rename(&backup, &file)?;
        let id = sm.append_message(&user_agent("retry me", 3))?;
        assert_eq!(sm.get_entries().len(), before_count + 1);
        assert_eq!(sm.get_leaf_id(), Some(id.as_str()));
        let persisted = fs::read_to_string(&file)?;
        assert_eq!(persisted.matches("retry me").count(), 1);
        Ok(())
    }

    #[test]
    fn invalid_file_preserved() -> TestResult {
        let dir = tempdir()?;
        let file = dir.path().join("bad.jsonl");
        let original = r#"{"type":"event","data":"not a session"}
"#;
        fs::write(&file, original)?;
        let Err(err) = SessionManager::open(path_str(&file)?, Some(path_str(dir.path())?), None)
        else {
            return Err("expected invalid session error".into());
        };
        match err {
            SessionError::InvalidSessionFile(p) => {
                assert!(p.contains("bad.jsonl"));
            }
            other => return Err(format!("wrong error: {other}").into()),
        }
        assert_eq!(fs::read_to_string(&file)?, original);
        Ok(())
    }

    #[test]
    fn empty_file_gets_header() -> TestResult {
        let dir = tempdir()?;
        let file = dir.path().join("empty.jsonl");
        fs::write(&file, "")?;
        let sm = SessionManager::open(path_str(&file)?, Some(path_str(dir.path())?), None)?;
        assert!(!sm.get_session_id().is_empty());
        let content = fs::read_to_string(&file)?;
        let header: Value = serde_json::from_str(content.trim())?;
        assert_eq!(header["type"], "session");
        assert_eq!(header["id"], sm.get_session_id());
        Ok(())
    }

    #[test]
    fn unknown_entry_roundtrip_and_excluded_from_context() -> TestResult {
        let dir = tempdir()?;
        let file = dir.path().join("unknown.jsonl");
        let original = concat!(
            r#"{"type":"session","version":3,"id":"sess-1","timestamp":"2025-01-01T00:00:00.000Z","cwd":"/tmp"}"#,
            "\n",
            r#"{"type":"future_thing","id":"u1","parentId":null,"timestamp":"2025-01-01T00:00:01.000Z","customField":{"a":1}}"#,
            "\n",
            r#"{"type":"message","id":"m1","parentId":"u1","timestamp":"2025-01-01T00:00:02.000Z","message":{"role":"user","content":"hi","timestamp":1}}"#,
            "\n",
            r#"{"type":"message","id":"m2","parentId":"m1","timestamp":"2025-01-01T00:00:03.000Z","message":{"role":"assistant","content":[{"type":"text","text":"yo"}],"api":"test","provider":"test","model":"test","usage":{"input":1,"output":1,"cacheRead":0,"cacheWrite":0,"totalTokens":2,"cost":{"input":0,"output":0,"cacheRead":0,"cacheWrite":0,"total":0}},"stopReason":"stop","timestamp":2}}"#,
            "\n",
        );
        fs::write(&file, original)?;

        let mut sm = SessionManager::open(path_str(&file)?, Some(path_str(dir.path())?), None)?;
        assert!(sm.get_entry("u1").is_some());
        assert_eq!(
            sm.get_entry("u1").map(SessionEntry::discriminant),
            Some("future_thing")
        );
        let ctx = sm.build_session_context()?;
        assert_eq!(ctx.messages.len(), 2); // unknown excluded
        assert_eq!(ctx.messages[0].role(), "user");

        sm.append_message(&user_agent("next", 3))?;
        let after = fs::read(&file)?;
        assert!(after.starts_with(original.as_bytes()));
        Ok(())
    }

    #[test]
    fn leaf_is_last_file_order_not_deepest() -> TestResult {
        let dir = tempdir()?;
        let file = dir.path().join("leaf.jsonl");
        // A → B → C (deep), then D child of A written last.
        let content = concat!(
            r#"{"type":"session","version":3,"id":"s","timestamp":"2025-01-01T00:00:00.000Z","cwd":"/tmp"}"#,
            "\n",
            r#"{"type":"message","id":"A","parentId":null,"timestamp":"2025-01-01T00:00:01.000Z","message":{"role":"user","content":"a","timestamp":1}}"#,
            "\n",
            r#"{"type":"message","id":"B","parentId":"A","timestamp":"2025-01-01T00:00:02.000Z","message":{"role":"assistant","content":[{"type":"text","text":"b"}],"api":"test","provider":"test","model":"test","usage":{"input":1,"output":1,"cacheRead":0,"cacheWrite":0,"totalTokens":2,"cost":{"input":0,"output":0,"cacheRead":0,"cacheWrite":0,"total":0}},"stopReason":"stop","timestamp":2}}"#,
            "\n",
            r#"{"type":"message","id":"C","parentId":"B","timestamp":"2025-01-01T00:00:03.000Z","message":{"role":"user","content":"c","timestamp":3}}"#,
            "\n",
            r#"{"type":"message","id":"D","parentId":"A","timestamp":"2025-01-01T00:00:04.000Z","message":{"role":"user","content":"d","timestamp":4}}"#,
            "\n",
        );
        fs::write(&file, content)?;
        let sm = SessionManager::open(path_str(&file)?, Some(path_str(dir.path())?), None)?;
        assert_eq!(sm.get_leaf_id(), Some("D"));
        let branch: Vec<&str> = sm.get_branch(None).iter().filter_map(|e| e.id()).collect();
        assert_eq!(branch, vec!["A", "D"]);
        Ok(())
    }

    #[test]
    fn labels_and_rechain_on_branched_session() -> TestResult {
        let mut sm = SessionManager::in_memory(Some("/tmp"), None)?;
        let msg1 = sm.append_message(&user_agent("hello", 1))?;
        sm.append_label_change(&msg1, Some("checkpoint"))?;
        let model = sm.append_model_change("anthropic", "claude-test")?;
        let msg2 = sm.append_message(&user_agent("followup", 2))?;

        sm.create_branched_session(&msg2)?;

        assert_eq!(
            sm.get_entry(&model).and_then(|e| e.parent_id()),
            Some(msg1.as_str())
        );
        assert_eq!(sm.get_label(&msg1), Some("checkpoint"));
        Ok(())
    }

    #[test]
    fn create_branched_session_defers_without_assistant() -> TestResult {
        let dir = tempdir()?;
        let mut sm =
            SessionManager::create(path_str(dir.path())?, Some(path_str(dir.path())?), None)?;
        let id1 = sm.append_message(&user_agent("first", 1))?;
        sm.append_message(&assistant_agent("answer", 2))?;
        sm.append_message(&user_agent("second", 3))?;
        sm.append_message(&assistant_agent("answer2", 4))?;

        let new_file = sm.create_branched_session(&id1)?.ok_or("path")?;
        assert!(
            !path_exists(Path::new(&new_file)),
            "no assistant on path → deferred"
        );
        sm.append_custom_entry("preset-state", Some(json!({"name": "plan"})))?;
        sm.append_message(&assistant_agent("new answer", 5))?;
        assert!(path_exists(Path::new(&new_file)));

        let content = fs::read_to_string(&new_file)?;
        let records: Vec<Value> = content
            .lines()
            .filter(|l| !l.is_empty())
            .map(serde_json::from_str)
            .collect::<Result<_, _>>()?;
        assert_eq!(
            records
                .iter()
                .filter(|r| r.get("type").and_then(Value::as_str) == Some("session"))
                .count(),
            1
        );
        let ids: Vec<&str> = records
            .iter()
            .filter(|r| r.get("type").and_then(Value::as_str) != Some("session"))
            .filter_map(|r| r.get("id").and_then(Value::as_str))
            .collect();
        let set: HashSet<&str> = ids.iter().copied().collect();
        assert_eq!(set.len(), ids.len(), "no duplicate ids");
        Ok(())
    }

    #[test]
    fn create_branched_session_writes_with_assistant() -> TestResult {
        let dir = tempdir()?;
        let mut sm =
            SessionManager::create(path_str(dir.path())?, Some(path_str(dir.path())?), None)?;
        sm.append_message(&user_agent("first", 1))?;
        let id2 = sm.append_message(&assistant_agent("answer", 2))?;
        sm.append_message(&user_agent("second", 3))?;
        sm.append_message(&assistant_agent("answer2", 4))?;

        let new_file = sm.create_branched_session(&id2)?.ok_or("path")?;
        assert!(path_exists(Path::new(&new_file)));
        Ok(())
    }

    #[test]
    fn fork_header_and_entries() -> TestResult {
        let dir = tempdir()?;
        let source = dir.path().join("source.jsonl");
        fs::write(
            &source,
            concat!(
                r#"{"type":"session","version":3,"id":"legacy-session-id","timestamp":"2025-01-01T00:00:00.000Z","cwd":"/old"}"#,
                "\n",
                r#"{"type":"message","id":"entry-1","parentId":null,"timestamp":"2025-01-01T00:00:01.000Z","message":{"role":"assistant","content":[{"type":"text","text":"hello"}],"api":"openai-responses","provider":"openai","model":"gpt-5.4","usage":{"input":0,"output":0,"cacheRead":0,"cacheWrite":0,"totalTokens":0,"cost":{"input":0,"output":0,"cacheRead":0,"cacheWrite":0,"total":0}},"stopReason":"stop","timestamp":1}}"#,
                "\n",
            ),
        )?;
        let forked = SessionManager::fork_from(
            path_str(&source)?,
            path_str(dir.path())?,
            Some(path_str(dir.path())?),
            None,
        )?;
        let header = forked.get_header().ok_or("header")?;
        assert_ne!(header.id.as_deref(), Some("legacy-session-id"));
        // uuid v7 shape
        let id = header.id.as_deref().ok_or("id")?;
        assert!(id.contains('-'));
        assert_eq!(header.parent_session.as_deref(), Some(path_str(&source)?));
        assert!(header.cwd.is_some());
        // Entry ids preserved
        assert!(forked.get_entry("entry-1").is_some());
        Ok(())
    }

    #[test]
    fn encoded_cwd() {
        assert_eq!(
            encode_cwd_for_session_dir("/home/user/project"),
            "--home-user-project--"
        );
        assert_eq!(encode_cwd_for_session_dir("C:\\Users\\x"), "--C--Users-x--");
        let agent = Path::new("/tmp/agent");
        let dir = default_session_dir_path("/a/b", agent);
        assert!(dir.ends_with("sessions/--a-b--") || dir.contains("sessions/--a-b--"));
    }

    #[test]
    fn custom_session_id_and_filename() -> TestResult {
        let dir = tempdir()?;
        let sm = SessionManager::create(
            path_str(dir.path())?,
            Some(path_str(dir.path())?),
            Some(NewSessionOptions {
                id: Some("created-session-id".to_owned()),
                parent_session: None,
            }),
        )?;
        assert_eq!(sm.get_session_id(), "created-session-id");
        let file = sm.get_session_file().ok_or("file")?;
        let base = Path::new(file)
            .file_name()
            .ok_or("file name")?
            .to_string_lossy();
        // YYYY-MM-DDTHH-MM-SS-mmmZ_created-session-id.jsonl
        assert!(base.ends_with("_created-session-id.jsonl"));
        assert!(!path_exists(Path::new(file)));
        Ok(())
    }

    #[test]
    fn invalid_session_ids_rejected() -> TestResult {
        for id in [
            "", "-abc", "abc-", "_abc", "abc_", ".abc", "abc.", "abc/def", "abc\\def", "abc def",
        ] {
            let mut sm = SessionManager::in_memory(Some("/tmp"), None)?;
            let Err(err) = sm.new_session(Some(NewSessionOptions {
                id: Some(id.to_owned()),
                parent_session: None,
            })) else {
                return Err(format!("expected invalid id for {id:?}").into());
            };
            assert!(matches!(err, SessionError::InvalidSessionId));
        }
        Ok(())
    }

    #[test]
    fn tree_and_branch() -> TestResult {
        let mut sm = SessionManager::in_memory(Some("/tmp"), None)?;
        let id1 = sm.append_message(&user_agent("1", 1))?;
        let id2 = sm.append_message(&assistant_agent("2", 2))?;
        let id3 = sm.append_message(&user_agent("3", 3))?;
        assert_eq!(sm.get_leaf_id(), Some(id3.as_str()));

        sm.branch(&id2)?;
        let id4 = sm.append_message(&user_agent("4-branch", 4))?;
        let tree = sm.get_tree();
        assert_eq!(tree.len(), 1);
        assert_eq!(tree[0].entry.id(), Some(id1.as_str()));
        let node2 = &tree[0].children[0];
        assert_eq!(node2.entry.id(), Some(id2.as_str()));
        assert_eq!(node2.children.len(), 2);
        let child_ids: HashSet<&str> = node2.children.iter().filter_map(|c| c.entry.id()).collect();
        assert!(child_ids.contains(id3.as_str()));
        assert!(child_ids.contains(id4.as_str()));
        Ok(())
    }

    #[test]
    fn model_and_thinking_context() -> TestResult {
        let mut sm = SessionManager::in_memory(Some("/tmp"), None)?;
        sm.append_message(&user_agent("hello", 1))?;
        sm.append_thinking_level_change("high")?;
        sm.append_model_change("openai", "gpt-4")?;
        sm.append_message(&assistant_agent("hi", 2))?;
        let ctx = sm.build_session_context()?;
        assert_eq!(ctx.thinking_level, "high");
        // assistant overwrites model_change
        let model = ctx.model.ok_or("model")?;
        assert_eq!(model.provider, "anthropic");
        assert_eq!(model.model_id, "claude-test");
        assert_eq!(ctx.messages.len(), 2);
        Ok(())
    }

    #[test]
    fn compaction_reconstruction() -> TestResult {
        let mut sm = SessionManager::in_memory(Some("/tmp"), None)?;
        let id1 = sm.append_message(&user_agent("first", 1))?;
        sm.append_message(&assistant_agent("r1", 2))?;
        let id3 = sm.append_message(&user_agent("second", 3))?;
        sm.append_message(&assistant_agent("r2", 4))?;
        sm.append_compaction("Summary of first two turns", &id3, 1000, None, None, None)?;
        sm.append_message(&user_agent("third", 5))?;
        sm.append_message(&assistant_agent("r3", 6))?;

        let ids: Vec<&str> = sm
            .build_context_entries()
            .iter()
            .filter_map(|e| e.id())
            .collect();
        // compaction + kept from id3 + post
        let compaction_id = sm
            .get_entries()
            .iter()
            .find(|e| e.discriminant() == "compaction")
            .and_then(|e| e.id())
            .ok_or("compaction entry")?;
        assert_eq!(ids[0], compaction_id);
        assert!(ids.contains(&id3.as_str()));
        assert!(!ids.contains(&id1.as_str())); // summarized away

        let ctx = sm.build_session_context()?;
        assert_eq!(ctx.messages[0].role(), "compactionSummary");
        assert_eq!(ctx.messages.len(), 5);
        Ok(())
    }

    #[test]
    fn summary_entries_persist_optional_usage_and_accept_null() -> TestResult {
        let usage = Usage {
            input: 10,
            output: 5,
            cache_read: 2,
            cache_write: 1,
            cache_write1h: Some(3),
            reasoning: Some(4),
            total_tokens: 18,
            cost: UsageCost {
                input: 0.1,
                output: 0.2,
                cache_read: 0.03,
                cache_write: 0.04,
                total: 0.37,
            },
        };
        let mut sm = SessionManager::in_memory(Some("/tmp"), None)?;
        let root = sm.append_message(&user_agent("root", 1))?;
        let compaction_id =
            sm.append_compaction("summary", &root, 18, None, None, Some(usage.clone()))?;
        let compaction = sm.get_entry(&compaction_id).ok_or("compaction entry")?;
        assert_eq!(
            serde_json::to_value(compaction)?["usage"],
            serde_json::to_value(&usage)?
        );

        let branch_id = sm.branch_with_summary(
            Some(&compaction_id),
            "branch",
            None,
            None,
            Some(usage.clone()),
        )?;
        let branch = sm.get_entry(&branch_id).ok_or("branch entry")?;
        assert_eq!(
            serde_json::to_value(branch)?["usage"],
            serde_json::to_value(&usage)?
        );

        let absent_id = sm.branch_with_summary(Some(&branch_id), "no usage", None, None, None)?;
        let absent = sm.get_entry(&absent_id).ok_or("absent usage entry")?;
        assert!(serde_json::to_value(absent)?.get("usage").is_none());

        let legacy: SessionEntry = serde_json::from_value(json!({
            "type": "compaction",
            "id": "legacy",
            "parentId": null,
            "timestamp": "2025-01-01T00:00:00.000Z",
            "summary": "legacy",
            "firstKeptEntryId": "root",
            "tokensBefore": 0,
            "usage": null
        }))?;
        match legacy {
            SessionEntry::Compaction(entry) => assert!(entry.usage.is_none()),
            other => return Err(format!("expected compaction, got {other:?}").into()),
        }
        Ok(())
    }

    #[test]
    fn custom_entries_in_tree_not_context() -> TestResult {
        let mut sm = SessionManager::in_memory(Some("/tmp"), None)?;
        let msg_id = sm.append_message(&user_agent("hello", 1))?;
        let custom_id = sm.append_custom_entry("my_data", Some(json!({"foo": "bar"})))?;
        sm.append_message(&assistant_agent("hi", 2))?;

        let entries = sm.get_entries();
        assert_eq!(entries.len(), 3);
        assert_eq!(entries[1].discriminant(), "custom");
        assert_eq!(entries[1].id(), Some(custom_id.as_str()));
        assert_eq!(entries[1].parent_id(), Some(msg_id.as_str()));

        let path = sm.get_branch(None);
        assert_eq!(path.len(), 3);

        let ctx = sm.build_session_context()?;
        assert_eq!(ctx.messages.len(), 2);
        Ok(())
    }

    #[test]
    fn label_not_found_throws() -> TestResult {
        let mut sm = SessionManager::in_memory(Some("/tmp"), None)?;
        let Err(err) = sm.append_label_change("non-existent", Some("label")) else {
            return Err("expected entry not found".into());
        };
        assert!(matches!(err, SessionError::EntryNotFound(_)));
        assert_eq!(err.to_string(), "Entry non-existent not found");
        Ok(())
    }

    #[test]
    fn branch_not_found_throws() -> TestResult {
        let mut sm = SessionManager::in_memory(Some("/tmp"), None)?;
        sm.append_message(&user_agent("hello", 1))?;
        let Err(err) = sm.branch("nonexistent") else {
            return Err("expected entry not found".into());
        };
        assert_eq!(err.to_string(), "Entry nonexistent not found");
        Ok(())
    }

    #[test]
    #[allow(clippy::too_many_lines)]
    fn generated_cross_version_session_interoperability() -> TestResult {
        fn fixture_files(
            root: &Path,
            files: &mut Vec<PathBuf>,
        ) -> Result<(), Box<dyn std::error::Error>> {
            for entry in fs::read_dir(root)? {
                let path = entry?.path();
                if path.is_dir() {
                    fixture_files(&path, files)?;
                } else if path.extension().and_then(|ext| ext.to_str()) == Some("jsonl") {
                    files.push(path);
                }
            }
            Ok(())
        }
        fn tree_ids(nodes: &[SessionTreeNode], ids: &mut Vec<String>) {
            for node in nodes {
                if let Some(id) = node.entry.id() {
                    ids.push(id.to_owned());
                }
                tree_ids(&node.children, ids);
            }
        }
        fn expected_tree_ids(value: &Value, ids: &mut Vec<String>) {
            for node in value.as_array().into_iter().flatten() {
                if let Some(id) = node
                    .get("entry")
                    .and_then(|entry| entry.get("id"))
                    .and_then(Value::as_str)
                {
                    ids.push(id.to_owned());
                }
                if let Some(children) = node.get("children") {
                    expected_tree_ids(children, ids);
                }
            }
        }
        let fixture_root = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../.agent-tasks/pi-rust-rewrite/fixtures/sessions");
        // The verification harness sets PI_SESSION_INTEROP_OUTPUT so TypeScript
        // can reopen the Rust-produced files afterwards. When unset (plain
        // `cargo test`), fall back to a fresh per-run directory under the crate
        // temp area so the proof is self-contained and free of stale content.
        let _output_guard;
        let output_root: PathBuf = if let Some(dir) = std::env::var_os("PI_SESSION_INTEROP_OUTPUT")
        {
            _output_guard = None;
            PathBuf::from(dir)
        } else {
            let tmp_base = std::env::var_os("CARGO_TARGET_TMPDIR")
                .map_or_else(std::env::temp_dir, PathBuf::from);
            let guard = tempfile::tempdir_in(&tmp_base)?;
            let path = guard.path().to_path_buf();
            _output_guard = Some(guard);
            path
        };
        fs::create_dir_all(&output_root)?;
        let mut fixtures = Vec::new();
        if !fixture_root.is_dir() {
            return Err(format!(
                "session fixture directory not found: {}\n\
                 run `bun run scripts/generate-session-fixtures.ts` before this proof",
                fixture_root.display()
            )
            .into());
        }
        // Derive the expected fixture count from the generator's authoritative
        // manifest (written next to the fixtures by
        // scripts/generate-session-fixtures.ts) rather than scraping the
        // generator source at compile time.
        let manifest_path = fixture_root.join("manifest.json");
        let expected_fixture_count: usize = {
            let manifest_bytes = fs::read(&manifest_path).map_err(|err| {
                format!(
                    "session fixture manifest not found: {}\n\
                     run `bun run scripts/generate-session-fixtures.ts` to regenerate ({err})",
                    manifest_path.display()
                )
            })?;
            let manifest: Value = serde_json::from_slice(&manifest_bytes).map_err(|err| {
                format!(
                    "invalid session fixture manifest {}: {err}",
                    manifest_path.display()
                )
            })?;
            let count = manifest
                .get("count")
                .and_then(Value::as_u64)
                .ok_or_else(|| {
                    format!(
                        "session fixture manifest {} has no numeric `count` field",
                        manifest_path.display()
                    )
                })?;
            usize::try_from(count).map_err(|_| {
                format!(
                    "session fixture manifest {} `count` overflows usize",
                    manifest_path.display()
                )
            })?
        };
        fixture_files(&fixture_root, &mut fixtures)?;
        fixtures.sort();
        assert_eq!(
            fixtures.len(),
            expected_fixture_count,
            "expected {expected_fixture_count} session fixtures under {}, found {}; \
             run `bun run scripts/generate-session-fixtures.ts` to regenerate",
            fixture_root.display(),
            fixtures.len()
        );

        let mut saw_v1 = false;
        let mut saw_v2 = false;
        let mut saw_v3 = false;
        let mut saw_unknown = false;
        for fixture in fixtures {
            let expected_path = fixture.with_extension("expected.json");
            let expected: Value = serde_json::from_slice(&fs::read(&expected_path)?)?;
            let fixture_name = expected["fixture"].as_str().ok_or("fixture name")?;
            match expected["formatVersion"].as_u64().ok_or("format version")? {
                1 => saw_v1 = true,
                2 => saw_v2 = true,
                3 => saw_v3 = true,
                version => return Err(format!("unexpected fixture version {version}").into()),
            }

            let relative = fixture.strip_prefix(&fixture_root)?;
            let scenario_dir = output_root.join(relative).with_extension("");
            fs::create_dir_all(&scenario_dir)?;
            let continued = scenario_dir.join("continued.jsonl");
            fs::copy(&fixture, &continued)?;
            let mut session =
                SessionManager::open(path_str(&continued)?, Some(path_str(&scenario_dir)?), None)?;
            let migrated_prefix = fs::read(&continued)?;

            let header = session.get_header().ok_or("header")?;
            assert_eq!(
                header.version,
                Some(CURRENT_SESSION_VERSION),
                "{fixture_name}: header version"
            );
            assert_eq!(
                header.id.as_deref(),
                expected["sessionId"].as_str(),
                "{fixture_name}: header id"
            );
            let entries: Vec<Value> = session
                .get_entries()
                .into_iter()
                .map(serde_json::to_value)
                .collect::<Result<_, _>>()?;
            let expected_entries = expected["entries"].as_array().ok_or("expected entries")?;
            assert_eq!(
                entries.len(),
                expected_entries.len(),
                "{fixture_name}: entry count"
            );
            let mut expected_to_actual = HashMap::new();
            for (actual, expected_entry) in entries.iter().zip(expected_entries) {
                assert_eq!(
                    actual["type"], expected_entry["type"],
                    "{fixture_name}: entry type"
                );
                let expected_id = expected_entry["id"].as_str().ok_or("expected entry id")?;
                let actual_id = actual["id"].as_str().ok_or("actual entry id")?;
                if let Some(expected_message) = expected_entry.get("message") {
                    let actual_message = actual.get("message").ok_or("actual message")?;
                    for key in ["role", "content", "api", "provider", "model", "stopReason"] {
                        if !expected_message[key].is_null() {
                            assert_eq!(
                                actual_message[key], expected_message[key],
                                "{fixture_name}: message {key}"
                            );
                        }
                    }
                }
                expected_to_actual.insert(expected_id.to_owned(), actual_id.to_owned());
            }
            assert_eq!(
                session.get_leaf_id(),
                expected["leaf"]
                    .as_str()
                    .and_then(|id| expected_to_actual.get(id).map(String::as_str)),
                "{fixture_name}: leaf",
            );
            assert_eq!(
                session.get_session_name(),
                expected["name"].as_str().map(str::to_owned),
                "{fixture_name}: name"
            );
            for (id, label) in expected["labels"].as_object().ok_or("labels")? {
                assert_eq!(
                    session.get_label(expected_to_actual.get(id).ok_or("label target")?),
                    label.as_str(),
                    "{fixture_name}: label {id}",
                );
            }
            let mut actual_tree = Vec::new();
            tree_ids(&session.get_tree(), &mut actual_tree);
            let mut expected_tree = Vec::new();
            expected_tree_ids(&expected["tree"], &mut expected_tree);
            let actual_tree_as_expected: Vec<String> = actual_tree
                .into_iter()
                .map(|actual| {
                    expected_to_actual
                        .iter()
                        .find_map(|(expected, mapped)| {
                            (*mapped == actual).then_some(expected.clone())
                        })
                        .ok_or("tree entry")
                })
                .collect::<Result<_, _>>()?;
            assert_eq!(
                actual_tree_as_expected, expected_tree,
                "{fixture_name}: tree"
            );
            let context = session.build_session_context()?;
            let expected_context = expected["context"]["messages"]
                .as_array()
                .ok_or("context messages")?;
            let actual_context: Vec<Value> = context
                .messages
                .iter()
                .map(serde_json::to_value)
                .collect::<Result<_, _>>()?;
            assert_eq!(
                actual_context.len(),
                expected_context.len(),
                "{fixture_name}: context"
            );
            for (actual, expected_message) in actual_context.iter().zip(expected_context) {
                assert_eq!(
                    actual["role"], expected_message["role"],
                    "{fixture_name}: context role"
                );
                assert_eq!(
                    actual["content"], expected_message["content"],
                    "{fixture_name}: context content"
                );
            }
            assert_eq!(
                session
                    .get_entries()
                    .iter()
                    .filter(|entry| entry.discriminant() == "compaction")
                    .count(),
                expected["entries"]
                    .as_array()
                    .ok_or("expected entries")?
                    .iter()
                    .filter(|entry| entry["type"] == "compaction")
                    .count(),
                "{fixture_name}: compaction",
            );
            saw_unknown |= entries.iter().any(|entry| entry["type"] == "future_thing");

            session.append_message(&assistant_agent("Rust interop continuation", 42))?;
            let continued_bytes = fs::read(&continued)?;
            assert!(
                continued_bytes.starts_with(&migrated_prefix),
                "{fixture_name}: continuation rewrote history"
            );
            let leaf = session.get_leaf_id().ok_or("continued leaf")?.to_owned();
            let forked = SessionManager::fork_from(
                path_str(&continued)?,
                path_str(&scenario_dir)?,
                Some(path_str(&scenario_dir)?),
                None,
            )?;
            let forked_file = forked.get_session_file().ok_or("forked file")?;
            assert!(
                Path::new(forked_file).exists(),
                "{fixture_name}: fork output missing"
            );
            let cloned_file = session
                .create_branched_session(&leaf)?
                .ok_or("cloned file")?;
            assert!(
                Path::new(&cloned_file).exists(),
                "{fixture_name}: clone output missing"
            );
        }
        assert!(
            saw_v1 && saw_v2 && saw_v3,
            "fixture set must cover v1/v2/v3"
        );
        assert!(
            saw_unknown,
            "fixture set must preserve opaque future entries"
        );
        Ok(())
    }
}
