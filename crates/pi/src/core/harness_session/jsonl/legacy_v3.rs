use std::collections::{HashMap, HashSet};
use std::fs;
use std::path::Path;
use std::sync::Arc;

use pi_agent::AgentMessage;
use pi_agent::session::{
    CommittedValueWrite, CommittedWrite, Entry, EntryId, IdGenerator, LaneState, ModelIdentity,
    NewEntry, NewEntryBody, SessionError, StorageErrorCode, StorageFailure,
};
use pi_ai::{Message, ModelThinkingLevel, Usage};
use serde_json::Value;

use crate::core::messages::{
    CustomMessageContent, create_branch_summary_message, create_compaction_summary_message,
    create_custom_message,
};
use crate::core::sessions::{FileEntry, SessionEntry};

use super::codec::{JsonlStorageHeader, LegacyV3Header, ParsedHeader, parse_header};

/// Result of normalizing a historical v3 session in memory.
#[derive(Clone, Debug)]
pub struct NormalizedLegacyV3Records {
    /// Canonical v4 writes representing the legacy conversation and values.
    pub writes: Vec<CommittedWrite>,
    /// Usage observed in legacy assistant/tool/summary records.
    pub imported_usage: Usage,
    /// Sequence high-water mark after the normalized baseline.
    pub next_seq: u64,
}

fn corrupt(
    message: impl Into<String>,
    source: Option<Arc<dyn std::error::Error + Send + Sync>>,
) -> SessionError {
    SessionError::Backend(StorageFailure {
        code: StorageErrorCode::Corrupt,
        message: message.into(),
        source,
    })
}
fn invalid_data(message: impl Into<String>) -> Arc<dyn std::error::Error + Send + Sync> {
    Arc::new(std::io::Error::new(
        std::io::ErrorKind::InvalidData,
        message.into(),
    ))
}

fn timestamp_millis(timestamp: &str) -> Result<i64, SessionError> {
    let millis = timestamp
        .parse::<jiff::Timestamp>()
        .map(jiff::Timestamp::as_millisecond)
        .map_err(|error| {
            corrupt(
                format!("invalid legacy v3 timestamp {timestamp}"),
                Some(Arc::new(error)),
            )
        })?;
    if millis < 0 {
        return Err(corrupt(
            format!("invalid legacy v3 timestamp {timestamp}"),
            None,
        ));
    }
    Ok(millis)
}

#[derive(Clone, Debug)]
struct LegacyRecord {
    kind: String,
    id: String,
    parent_id: Option<String>,
    timestamp: String,
    raw: Value,
    entry: SessionEntry,
}

fn accepted_kind(kind: &str) -> bool {
    matches!(
        kind,
        "message"
            | "custom"
            | "custom_message"
            | "branch_summary"
            | "compaction"
            | "model_change"
            | "thinking_level_change"
            | "active_tools_change"
            | "session_info"
            | "label"
    )
}

fn parse_record(line: &str, line_number: usize) -> Result<LegacyRecord, SessionError> {
    let raw: Value = serde_json::from_str(line).map_err(|error| {
        corrupt(
            format!("invalid legacy v3 JSONL record at line {line_number}: not valid JSON"),
            Some(Arc::new(error)),
        )
    })?;
    let kind = raw.get("type").and_then(Value::as_str).ok_or_else(|| {
        corrupt(
            format!("unsupported legacy v3 record type at line {line_number}"),
            None,
        )
    })?;
    if !accepted_kind(kind) {
        return Err(corrupt(
            format!("unsupported legacy v3 record type at line {line_number}: {kind}"),
            None,
        ));
    }
    let id = raw
        .get("id")
        .and_then(Value::as_str)
        .ok_or_else(|| {
            corrupt(
                format!("invalid legacy v3 record id at line {line_number}"),
                None,
            )
        })?
        .to_owned();
    let parent_id = match raw.get("parentId") {
        None | Some(Value::Null) => None,
        Some(Value::String(parent)) => Some(parent.clone()),
        Some(_) => {
            return Err(corrupt(
                format!("invalid legacy v3 parent id at line {line_number}"),
                None,
            ));
        }
    };
    let timestamp = raw
        .get("timestamp")
        .and_then(Value::as_str)
        .ok_or_else(|| {
            corrupt(
                format!("invalid legacy v3 timestamp at line {line_number}"),
                None,
            )
        })?
        .to_owned();
    let _ = timestamp_millis(&timestamp)?;

    let file_entry: FileEntry = serde_json::from_value(raw.clone()).map_err(|error| {
        corrupt(
            format!("invalid legacy v3 record at line {line_number}"),
            Some(Arc::new(error)),
        )
    })?;
    let FileEntry::Entry(entry) = file_entry else {
        return Err(corrupt(
            format!("unsupported legacy v3 record type at line {line_number}: {kind}"),
            None,
        ));
    };
    if matches!(&entry, SessionEntry::Unknown(_)) && kind != "active_tools_change" {
        return Err(corrupt(
            format!("invalid legacy v3 record at line {line_number}: {kind}"),
            Some(invalid_data("legacy record did not match its known schema")),
        ));
    }
    Ok(LegacyRecord {
        kind: kind.to_owned(),
        id,
        parent_id,
        timestamp,
        raw,
        entry,
    })
}

struct RetainedIdResolver<'a> {
    records: &'a HashMap<String, LegacyRecord>,
    reminted_ids: &'a HashMap<String, EntryId>,
    resolved: HashMap<String, Option<EntryId>>,
}

impl<'a> RetainedIdResolver<'a> {
    fn new(
        records: &'a HashMap<String, LegacyRecord>,
        reminted_ids: &'a HashMap<String, EntryId>,
    ) -> Self {
        Self {
            records,
            reminted_ids,
            resolved: HashMap::new(),
        }
    }

    fn resolve(&mut self, legacy_id: Option<&str>) -> Result<Option<EntryId>, SessionError> {
        let Some(mut current) = legacy_id.map(str::to_owned) else {
            return Ok(None);
        };
        let mut traversed = Vec::new();
        let mut visited = HashSet::new();
        let resolved = loop {
            if let Some(reminted) = self.reminted_ids.get(&current) {
                break Some(reminted.clone());
            }
            if let Some(cached) = self.resolved.get(&current) {
                break cached.clone();
            }
            if !visited.insert(current.clone()) {
                return Err(corrupt(
                    format!("cycle in legacy v3 parent chain at entry {current}"),
                    None,
                ));
            }
            let record = self.records.get(&current).ok_or_else(|| {
                corrupt(
                    format!("missing legacy v3 entry reference: {current}"),
                    None,
                )
            })?;
            traversed.push(current.clone());
            let Some(parent) = record.parent_id.as_deref() else {
                break None;
            };
            current = parent.to_owned();
        };
        for id in traversed {
            self.resolved.insert(id, resolved.clone());
        }
        Ok(resolved)
    }

    fn require(&mut self, legacy_id: &str) -> Result<EntryId, SessionError> {
        self.resolve(Some(legacy_id))?.ok_or_else(|| {
            corrupt(
                format!("legacy v3 entry reference has no retained ancestor: {legacy_id}"),
                None,
            )
        })
    }
}

fn custom_agent_message(
    custom_type: &str,
    content: CustomMessageContent,
    display: bool,
    details: Option<Value>,
    timestamp: &str,
) -> Result<AgentMessage, SessionError> {
    let message = create_custom_message(custom_type, content, display, details, timestamp)
        .map_err(|error| {
            corrupt(
                "legacy custom message conversion failed",
                Some(Arc::new(error)),
            )
        })?;
    let value = serde_json::to_value(message).map_err(|error| {
        corrupt(
            "legacy custom message serialization failed",
            Some(Arc::new(error)),
        )
    })?;
    serde_json::from_value(value).map_err(|error| {
        corrupt(
            "legacy custom message conversion failed",
            Some(Arc::new(error)),
        )
    })
}

fn summary_agent_message(
    summary: &str,
    from_id: Option<&EntryId>,
    timestamp: &str,
) -> Result<AgentMessage, SessionError> {
    let from_id = from_id.map_or_else(|| "root".to_owned(), ToString::to_string);
    let message = create_branch_summary_message(summary, &from_id, timestamp).map_err(|error| {
        corrupt(
            "legacy branch summary conversion failed",
            Some(Arc::new(error)),
        )
    })?;
    let value = serde_json::to_value(message).map_err(|error| {
        corrupt(
            "legacy branch summary serialization failed",
            Some(Arc::new(error)),
        )
    })?;
    serde_json::from_value(value).map_err(|error| {
        corrupt(
            "legacy branch summary conversion failed",
            Some(Arc::new(error)),
        )
    })
}

fn compaction_agent_message(
    summary: &str,
    tokens_before: i64,
    timestamp: &str,
) -> Result<AgentMessage, SessionError> {
    let message = create_compaction_summary_message(summary, tokens_before, timestamp)
        .map_err(|error| corrupt("legacy compaction conversion failed", Some(Arc::new(error))))?;
    let value = serde_json::to_value(message).map_err(|error| {
        corrupt(
            "legacy compaction serialization failed",
            Some(Arc::new(error)),
        )
    })?;
    serde_json::from_value(value)
        .map_err(|error| corrupt("legacy compaction conversion failed", Some(Arc::new(error))))
}

fn context_messages(
    record: &LegacyRecord,
    resolver: &mut RetainedIdResolver<'_>,
) -> Result<Vec<AgentMessage>, SessionError> {
    match &record.entry {
        SessionEntry::Message(entry) => Ok(vec![entry.message.clone()]),
        SessionEntry::CustomMessage(entry) => Ok(vec![custom_agent_message(
            &entry.custom_type,
            entry.content.clone(),
            entry.display,
            entry.details.clone(),
            &entry.timestamp,
        )?]),
        SessionEntry::BranchSummary(entry) if !entry.summary.is_empty() => {
            let from_id = if entry.from_id == "root" {
                None
            } else {
                resolver.resolve(Some(&entry.from_id))?
            };
            Ok(vec![summary_agent_message(
                &entry.summary,
                from_id.as_ref(),
                &entry.timestamp,
            )?])
        }
        SessionEntry::Compaction(entry) => Ok(vec![compaction_agent_message(
            &entry.summary,
            entry.tokens_before,
            &entry.timestamp,
        )?]),
        _ => Ok(Vec::new()),
    }
}

fn materialize_retained_tail(
    compaction: &LegacyRecord,
    records: &HashMap<String, LegacyRecord>,
    resolver: &mut RetainedIdResolver<'_>,
    first_kept_id: &str,
) -> Result<Vec<AgentMessage>, SessionError> {
    let mut reversed = Vec::new();
    let mut visited = HashSet::new();
    let mut current = compaction.parent_id.clone();
    while let Some(ref id) = current {
        if !visited.insert(id.clone()) {
            return Err(corrupt(
                format!("cycle in legacy v3 parent chain at entry {id}"),
                None,
            ));
        }
        let record = records
            .get(id)
            .ok_or_else(|| corrupt(format!("missing legacy v3 parent entry: {id}"), None))?;
        reversed.push(record.clone());
        if id == first_kept_id {
            let mut output = Vec::new();
            for record in reversed.iter().rev() {
                output.extend(context_messages(record, resolver)?);
            }
            return Ok(output);
        }
        current.clone_from(&record.parent_id);
    }
    Err(corrupt(
        format!(
            "legacy v3 compaction {} firstKeptEntryId is not on its parent branch: {first_kept_id}",
            compaction.id
        ),
        None,
    ))
}

fn normalize_retained_entry(
    record: &LegacyRecord,
    seq: u64,
    records: &HashMap<String, LegacyRecord>,
    resolver: &mut RetainedIdResolver<'_>,
) -> Result<Entry, SessionError> {
    let timestamp = timestamp_millis(&record.timestamp)?;
    let id = resolver.require(&record.id)?;
    let parent_id = resolver.resolve(record.parent_id.as_deref())?;
    let body = match &record.entry {
        SessionEntry::Message(entry) => NewEntryBody::Message {
            message: entry.message.clone(),
            terminate: false,
        },
        SessionEntry::CustomMessage(entry) => NewEntryBody::Message {
            message: custom_agent_message(
                &entry.custom_type,
                entry.content.clone(),
                entry.display,
                entry.details.clone(),
                &entry.timestamp,
            )?,
            terminate: false,
        },
        SessionEntry::BranchSummary(entry) => NewEntryBody::BranchSummary {
            from_id: if entry.from_id == "root" {
                None
            } else {
                resolver.resolve(Some(&entry.from_id))?
            },
            summary: entry.summary.clone(),
            details: entry.details.clone(),
            usage: entry.usage.clone(),
            from_hook: entry.from_hook.unwrap_or(false),
        },
        SessionEntry::Compaction(entry) => NewEntryBody::Compaction {
            summary: entry.summary.clone(),
            retained_tail: materialize_retained_tail(
                record,
                records,
                resolver,
                &entry.first_kept_entry_id,
            )?,
            tokens_before: u64::try_from(entry.tokens_before).map_err(|_| {
                corrupt(
                    format!("legacy compaction {} has a negative token count", record.id),
                    None,
                )
            })?,
            details: entry.details.clone(),
            usage: entry.usage.clone(),
            from_hook: entry.from_hook.unwrap_or(false),
        },
        SessionEntry::Custom(entry) => NewEntryBody::Custom {
            custom_type: entry.custom_type.clone(),
            data: entry.data.clone(),
        },
        _ => {
            return Err(corrupt(
                format!("legacy record {} is not retained", record.id),
                None,
            ));
        }
    };
    Ok(NewEntry {
        id,
        parent_id,
        body,
    }
    .materialize(seq, timestamp))
}

fn add_usage(total: &mut Usage, add: &Usage) {
    total.input = total.input.saturating_add(add.input);
    total.output = total.output.saturating_add(add.output);
    total.cache_read = total.cache_read.saturating_add(add.cache_read);
    total.cache_write = total.cache_write.saturating_add(add.cache_write);
    total.total_tokens = total.total_tokens.saturating_add(add.total_tokens);
    total.cache_write1h = match (total.cache_write1h, add.cache_write1h) {
        (Some(left), Some(right)) => Some(left.saturating_add(right)),
        (left, None) => left,
        (None, right) => right,
    };
    total.reasoning = match (total.reasoning, add.reasoning) {
        (Some(left), Some(right)) => Some(left.saturating_add(right)),
        (left, None) => left,
        (None, right) => right,
    };
    total.cost.input += add.cost.input;
    total.cost.output += add.cost.output;
    total.cost.cache_read += add.cost.cache_read;
    total.cost.cache_write += add.cost.cache_write;
    total.cost.total += add.cost.total;
}

fn usage_from_message(record: &LegacyRecord) -> Option<Usage> {
    let SessionEntry::Message(entry) = &record.entry else {
        return None;
    };
    match &entry.message {
        AgentMessage::Llm(message) => match message.as_ref() {
            Message::Assistant(assistant) => Some(assistant.usage.clone()),
            Message::ToolResult(_) => record
                .raw
                .get("message")
                .and_then(|message| message.get("usage"))
                .and_then(|usage| serde_json::from_value(usage.clone()).ok()),
            Message::User(_) => None,
        },
        AgentMessage::Custom(_) => None,
    }
}

fn aggregate_usage(records: &[LegacyRecord]) -> Usage {
    let mut total = Usage::default();
    for record in records {
        let usage = match record.kind.as_str() {
            "message" => usage_from_message(record),
            "compaction" => match &record.entry {
                SessionEntry::Compaction(entry) => entry.usage.clone(),
                _ => None,
            },
            "branch_summary" => match &record.entry {
                SessionEntry::BranchSummary(entry) => entry.usage.clone(),
                _ => None,
            },
            _ => None,
        };
        if let Some(usage) = usage {
            add_usage(&mut total, &usage);
        }
    }
    total
}

fn value_write(
    seq: &mut u64,
    namespace: &str,
    key: &str,
    value: Value,
) -> Result<CommittedWrite, SessionError> {
    let current = *seq;
    *seq = seq
        .checked_add(1)
        .ok_or_else(|| SessionError::Invariant("sequence overflow".to_owned()))?;
    Ok(CommittedWrite::Value(CommittedValueWrite::Set {
        seq: current,
        namespace: namespace.to_owned(),
        key: key.to_owned(),
        value,
    }))
}

fn selected_configuration(
    records: &HashMap<String, LegacyRecord>,
    final_id: Option<&str>,
) -> Result<Option<pi_agent::session::LaneConfiguration>, SessionError> {
    let mut model = None;
    let mut thinking_level = None;
    let mut active_tool_names = None;
    let mut saw_model = false;
    let mut saw_thinking_level = false;
    let mut saw_active_tool_names = false;
    let mut visited = HashSet::new();
    let mut current = final_id.map(str::to_owned);
    while let Some(ref id) = current {
        if saw_model && saw_thinking_level && saw_active_tool_names {
            break;
        }
        if !visited.insert(id.clone()) {
            return Err(corrupt("cycle in legacy v3 parent chain", None));
        }
        let record = records
            .get(id)
            .ok_or_else(|| corrupt(format!("missing legacy v3 parent entry: {id}"), None))?;
        match record.kind.as_str() {
            "model_change" if !saw_model => {
                saw_model = true;
                let provider = record.raw.get("provider").and_then(Value::as_str);
                let model_id = record.raw.get("modelId").and_then(Value::as_str);
                if let (Some(provider), Some(model_id)) = (provider, model_id)
                    && !provider.is_empty()
                    && !model_id.is_empty()
                {
                    model = Some(ModelIdentity {
                        provider: provider.to_owned(),
                        model_id: model_id.to_owned(),
                        api: None,
                    });
                }
            }
            "thinking_level_change" if !saw_thinking_level => {
                saw_thinking_level = true;
                if let Some(level) = record.raw.get("thinkingLevel").and_then(Value::as_str)
                    && let Ok(parsed) = serde_json::from_value::<ModelThinkingLevel>(Value::String(
                        level.to_owned(),
                    ))
                {
                    thinking_level = Some(parsed);
                }
            }
            "active_tools_change" if !saw_active_tool_names => {
                saw_active_tool_names = true;
                if let Some(names) = record.raw.get("activeToolNames").and_then(Value::as_array)
                    && names.iter().all(Value::is_string)
                {
                    active_tool_names = Some(
                        names
                            .iter()
                            .filter_map(Value::as_str)
                            .map(str::to_owned)
                            .collect(),
                    );
                }
            }
            _ => {}
        }
        current.clone_from(&record.parent_id);
    }
    Ok(model.zip(thinking_level).map(|(model, thinking_level)| {
        pi_agent::session::LaneConfiguration {
            model,
            thinking_level,
            active_tool_names: active_tool_names.unwrap_or_default(),
        }
    }))
}

/// Normalizes v3 records without touching the source file.
///
/// # Errors
///
/// Returns an error for malformed records or timestamps, duplicate ids, missing
/// or cyclic references, invalid compaction or configuration data, identifier
/// generation failures, serialization failures, or sequence overflow.
#[expect(
    clippy::too_many_lines,
    reason = "single-pass index, link, and validate over legacy records; splitting would scatter the phases"
)]
pub fn normalize_legacy_v3_records(
    record_lines: &[&str],
) -> Result<NormalizedLegacyV3Records, SessionError> {
    let mut records = Vec::with_capacity(record_lines.len());
    let mut by_id = HashMap::with_capacity(record_lines.len());
    for (index, line) in record_lines.iter().enumerate() {
        let record = parse_record(line, index + 2)?;
        if by_id.insert(record.id.clone(), record.clone()).is_some() {
            return Err(corrupt(
                format!("duplicate legacy v3 entry id: {}", record.id),
                None,
            ));
        }
        records.push(record);
    }

    let retained: Vec<&LegacyRecord> = records
        .iter()
        .filter(|record| {
            matches!(
                record.kind.as_str(),
                "message" | "custom" | "custom_message" | "branch_summary" | "compaction"
            )
        })
        .collect();
    let generator = pi_agent::session::UuidV7Generator::new();
    let mut reminted_ids = HashMap::with_capacity(retained.len());
    for record in &retained {
        let timestamp = timestamp_millis(&record.timestamp)?;
        let id = generator.next(Some(timestamp))?;
        reminted_ids.insert(record.id.clone(), EntryId::from(id));
    }
    let mut resolver = RetainedIdResolver::new(&by_id, &reminted_ids);
    let mut writes = Vec::with_capacity(retained.len().saturating_add(4));
    for (index, record) in retained.iter().enumerate() {
        let entry = normalize_retained_entry(
            record,
            u64::try_from(index).unwrap_or(u64::MAX).saturating_add(1),
            &by_id,
            &mut resolver,
        )?;
        writes.push(CommittedWrite::Entry(entry));
    }

    let mut next_seq = u64::try_from(writes.len())
        .map_err(|_| SessionError::Invariant("sequence overflow".to_owned()))?
        .checked_add(1)
        .ok_or_else(|| SessionError::Invariant("sequence overflow".to_owned()))?;
    let latest_name = records.iter().rev().find_map(|record| match &record.entry {
        SessionEntry::SessionInfo(entry) => Some(entry.name.clone()),
        _ => None,
    });
    if let Some(Some(name)) = latest_name
        && !name.is_empty()
    {
        writes.push(value_write(
            &mut next_seq,
            "pi.session.name",
            "",
            Value::String(name),
        )?);
    }

    let mut labels: Vec<(String, String)> = Vec::new();
    for record in &records {
        let SessionEntry::Label(entry) = &record.entry else {
            continue;
        };
        let Some(target) = resolver.resolve(Some(&entry.target_id))? else {
            continue;
        };
        if let Some(label) = entry.label.as_deref().filter(|label| !label.is_empty()) {
            if let Some(existing) = labels
                .iter_mut()
                .find(|(id, _)| id.as_str() == target.as_str())
            {
                label.clone_into(&mut existing.1);
            } else {
                labels.push((target.to_string(), label.to_owned()));
            }
        } else {
            labels.retain(|(id, _)| id.as_str() != target.as_str());
        }
    }
    for (target, label) in labels {
        writes.push(value_write(
            &mut next_seq,
            "pi.entry.label",
            &target,
            Value::String(label),
        )?);
    }

    let final_tip = records
        .last()
        .map(|record| resolver.resolve(Some(&record.id)))
        .transpose()?
        .flatten();
    writes.push(value_write(
        &mut next_seq,
        "pi.branch.tip",
        "main",
        serde_json::to_value(&final_tip).map_err(|error| {
            corrupt(
                "legacy branch tip serialization failed",
                Some(Arc::new(error)),
            )
        })?,
    )?);
    if let Some(configuration) =
        selected_configuration(&by_id, records.last().map(|record| record.id.as_str()))?
    {
        writes.push(value_write(
            &mut next_seq,
            "pi.lane.config",
            "main",
            serde_json::to_value(configuration).map_err(|error| {
                corrupt(
                    "legacy lane configuration serialization failed",
                    Some(Arc::new(error)),
                )
            })?,
        )?);
        writes.push(value_write(
            &mut next_seq,
            "pi.lane.state",
            "main",
            serde_json::to_value(LaneState::default()).map_err(|error| {
                corrupt(
                    "legacy lane state serialization failed",
                    Some(Arc::new(error)),
                )
            })?,
        )?);
    }

    Ok(NormalizedLegacyV3Records {
        next_seq,
        imported_usage: aggregate_usage(&records),
        writes,
    })
}

/// Normalizes legacy metadata in memory. The source file is only read when a
/// parent id can be resolved; it is never rewritten.
///
/// # Errors
///
/// Returns `Corrupt` if the header timestamp is invalid or predates the Unix
/// epoch. An unreadable or invalid parent file is retained as an unresolved path.
pub fn normalize_legacy_v3_header(
    path: &Path,
    header: &LegacyV3Header,
) -> Result<JsonlStorageHeader, SessionError> {
    let created_at = timestamp_millis(&header.timestamp)?;
    let mut parent_session_id = None;
    let mut legacy_parent_session_path = None;
    if let Some(parent_path) = header.parent_session.as_deref() {
        let resolved = fs::read_to_string(parent_path).ok().and_then(|content| {
            let first_line = content.lines().next()?;
            match parse_header(first_line).ok()? {
                ParsedHeader::V4(header) => Some(header.id),
                ParsedHeader::LegacyV3(header) => Some(header.id),
            }
        });
        if resolved.is_some() {
            parent_session_id = resolved;
        } else {
            legacy_parent_session_path = Some(parent_path.to_owned());
        }
    }
    let _ = path;
    Ok(JsonlStorageHeader {
        v: super::codec::JSONL_FORMAT_VERSION,
        kind: "header".to_owned(),
        id: header.id.clone(),
        storage_version: super::codec::JSONL_STORAGE_VERSION,
        created_at,
        cwd: header.cwd.clone(),
        parent_session_id,
        legacy_parent_session_path,
        next_seq: None,
    })
}
