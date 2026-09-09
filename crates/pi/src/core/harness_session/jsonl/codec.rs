use std::fs::{self, OpenOptions};
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use pi_agent::context::Context;
use pi_agent::session::{CommittedWrite, SessionError, StorageErrorCode, StorageFailure};
use serde::Deserialize;
use serde_json::Value;

/// JSONL format version emitted by the durable harness session backend.
pub const JSONL_FORMAT_VERSION: u32 = 4;
/// JSONL storage schema version supported by this backend.
pub const JSONL_STORAGE_VERSION: u32 = 1;
const MAX_SAFE_INTEGER: u64 = 9_007_199_254_740_991;

/// Header written as the first record of every JSONL session file.
#[derive(Clone, Debug, PartialEq, serde::Deserialize, serde::Serialize)]
pub struct JsonlStorageHeader {
    /// Wire-format version.
    pub v: u32,
    /// Header record discriminator (`"header"`).
    pub kind: String,
    /// Session identity.
    pub id: String,
    /// Backend storage version.
    #[serde(rename = "storageVersion")]
    pub storage_version: u32,
    /// Creation time in Unix milliseconds.
    #[serde(rename = "createdAt")]
    pub created_at: i64,
    /// Resolved working directory.
    pub cwd: String,
    /// Parent session identity, when this file was forked.
    #[serde(rename = "parentSessionId", skip_serializing_if = "Option::is_none")]
    pub parent_session_id: Option<String>,
    /// Legacy parent path retained when importing a v3 session.
    #[serde(
        rename = "legacyParentSessionPath",
        skip_serializing_if = "Option::is_none"
    )]
    pub legacy_parent_session_path: Option<String>,
    /// Sequence high-water mark retained by snapshot rewrites.
    #[serde(rename = "nextSeq", skip_serializing_if = "Option::is_none")]
    pub next_seq: Option<u64>,
}

/// Header shape used by the historical product session format.
#[derive(Clone, Debug, Deserialize)]
pub struct LegacyV3Header {
    /// Historical wire discriminator.
    #[serde(rename = "type")]
    pub kind: String,
    /// Historical file version.
    pub version: u32,
    /// Session identifier.
    pub id: String,
    /// ISO-8601 creation time.
    pub timestamp: String,
    /// Session working directory.
    pub cwd: String,
    /// Optional historical parent-session path.
    #[serde(rename = "parentSession")]
    pub parent_session: Option<String>,
}

/// Header family recognized by the JSONL backend.
#[derive(Clone, Debug)]
pub enum ParsedHeader {
    /// Current format-four header.
    V4(JsonlStorageHeader),
    /// Historical product format-three header.
    LegacyV3(LegacyV3Header),
}

fn failure(
    code: StorageErrorCode,
    message: impl Into<String>,
    source: Option<Arc<dyn std::error::Error + Send + Sync>>,
) -> SessionError {
    SessionError::Backend(StorageFailure {
        code,
        message: message.into(),
        source,
    })
}

fn invalid_header(
    message: impl Into<String>,
    source: Option<Arc<dyn std::error::Error + Send + Sync>>,
) -> SessionError {
    failure(StorageErrorCode::InvalidHeader, message, source)
}

/// Parses one JSONL session header.
///
/// The current header is accepted even when its storage version is newer or
/// older than this implementation; the open path performs that explicit
/// version gate so callers can distinguish malformed headers from unsupported
/// storage layouts.
///
/// # Errors
///
/// Returns `InvalidHeader` for malformed JSON, an unrecognized header shape, or
/// invalid header fields or timestamps.
pub fn parse_header(line: &str) -> Result<ParsedHeader, SessionError> {
    let value: Value = serde_json::from_str(line).map_err(|error| {
        invalid_header(
            "invalid JSONL session header: not valid JSON",
            Some(Arc::new(error)),
        )
    })?;
    let Some(object) = value.as_object() else {
        return Err(invalid_header("invalid JSONL session header", None));
    };

    if object.get("kind").and_then(Value::as_str) == Some("header")
        && object.get("v").and_then(Value::as_u64) == Some(u64::from(JSONL_FORMAT_VERSION))
    {
        let header: JsonlStorageHeader =
            serde_json::from_value(value.clone()).map_err(|error| {
                invalid_header("invalid JSONL session header", Some(Arc::new(error)))
            })?;
        let created_at = u64::try_from(header.created_at).ok();
        let next_seq_safe = header
            .next_seq
            .is_none_or(|next_seq| next_seq <= MAX_SAFE_INTEGER);
        if header.v != JSONL_FORMAT_VERSION
            || header.kind != "header"
            || header.storage_version == 0
            || u64::from(header.storage_version) > MAX_SAFE_INTEGER
            || created_at.is_none_or(|created_at| created_at > MAX_SAFE_INTEGER)
            || header.next_seq == Some(0)
            || !next_seq_safe
        {
            return Err(invalid_header("invalid JSONL session header", None));
        }
        return Ok(ParsedHeader::V4(header));
    }

    if object.get("type").and_then(Value::as_str) == Some("session")
        && object.get("version").and_then(Value::as_u64) == Some(3)
        && object.get("id").is_some_and(Value::is_string)
        && object.get("cwd").is_some_and(Value::is_string)
        && object.get("timestamp").is_some_and(Value::is_string)
        && object.get("parentSession").is_none_or(Value::is_string)
    {
        let header: LegacyV3Header = serde_json::from_value(value).map_err(|error| {
            invalid_header(
                "invalid legacy v3 JSONL session header",
                Some(Arc::new(error)),
            )
        })?;
        if header.timestamp.parse::<jiff::Timestamp>().is_err() {
            return Err(invalid_header(
                "invalid legacy v3 JSONL session header",
                None,
            ));
        }
        return Ok(ParsedHeader::LegacyV3(header));
    }

    Err(invalid_header("invalid JSONL session header", None))
}

fn wire_corrupt(
    message: impl Into<String>,
    source: Option<Arc<dyn std::error::Error + Send + Sync>>,
) -> SessionError {
    failure(StorageErrorCode::Corrupt, message, source)
}

fn validate_committed_wire(write: &CommittedWrite) -> Result<(), SessionError> {
    let seq = write.seq();
    if seq == 0 || seq > MAX_SAFE_INTEGER {
        return Err(wire_corrupt("invalid JSONL write seq", None));
    }
    if let CommittedWrite::Entry(entry) = write {
        let timestamp = u64::try_from(entry.timestamp()).ok();
        if timestamp.is_none_or(|timestamp| timestamp > MAX_SAFE_INTEGER) {
            return Err(wire_corrupt("invalid JSONL entry timestamp", None));
        }
    }
    Ok(())
}

/// Parses one transaction line, accepting either one write object or an array.
///
/// # Errors
///
/// Returns `Corrupt` for malformed JSON or writes with invalid fields, sequence
/// numbers, or timestamps.
pub fn parse_transaction(line: &str) -> Result<Vec<CommittedWrite>, SessionError> {
    let value: Value = serde_json::from_str(line).map_err(|error| {
        wire_corrupt(
            "invalid JSONL transaction: not valid JSON",
            Some(Arc::new(error)),
        )
    })?;
    let values = match value {
        Value::Array(values) => values,
        value => vec![value],
    };
    let mut writes = Vec::with_capacity(values.len());
    for value in values {
        let write: CommittedWrite = serde_json::from_value(value).map_err(|error| {
            wire_corrupt("invalid JSONL transaction write", Some(Arc::new(error)))
        })?;
        validate_committed_wire(&write)?;
        writes.push(write);
    }
    Ok(writes)
}

/// Serializes one transaction without a trailing newline.
///
/// A single write uses the compact object form from source C; larger batches
/// use an array. `CommittedWrite` contains only JSON values whose serialization
/// is infallible in practice, so a serialization failure is treated as a
/// violated internal invariant rather than emitted as a malformed record.
///
/// # Panics
///
/// Panics if serialization violates the internal invariant that committed
/// writes contain only JSON-serializable values.
#[must_use]
#[expect(
    clippy::expect_used,
    reason = "the String-returning API preserves invariant panics; committed writes contain only JSON-serializable values"
)]
pub fn serialize_transaction(writes: &[CommittedWrite]) -> String {
    if writes.len() == 1 {
        serde_json::to_string(&writes[0]).expect("committed JSONL writes must serialize")
    } else {
        serde_json::to_string(writes).expect("committed JSONL writes must serialize")
    }
}

/// Splits a file into complete newline-terminated records and reports a torn
/// final record. Empty content has no complete lines and is considered torn.
#[must_use]
pub fn split_complete_lines(content: &str) -> (Vec<&str>, bool) {
    if content.ends_with('\n') {
        return (
            content[..content.len().saturating_sub(1)]
                .split('\n')
                .collect(),
            false,
        );
    }
    let Some(last_newline) = content.rfind('\n') else {
        return (Vec::new(), true);
    };
    (content[..last_newline].split('\n').collect(), true)
}

/// Atomically publishes a complete JSONL file through a sibling temporary path.
///
/// The temporary file is removed after every failed stage. The caller owns the
/// serialized content and must include its final newline.
///
/// # Errors
///
/// Returns `Io` if creating directories, opening, writing, flushing, or renaming
/// the temporary file fails.
pub fn publish_file_atomically(path: &Path, content: &str) -> Result<(), SessionError> {
    let mut temp_path = path.as_os_str().to_os_string();
    temp_path.push(".tmp");
    let temp_path = PathBuf::from(temp_path);
    let result = (|| -> io::Result<()> {
        if let Some(parent) = path
            .parent()
            .filter(|parent| !parent.as_os_str().is_empty())
        {
            fs::create_dir_all(parent)?;
        }
        let mut file = OpenOptions::new()
            .create(true)
            .truncate(true)
            .write(true)
            .open(&temp_path)?;
        file.write_all(content.as_bytes())?;
        file.flush()?;
        drop(file);
        fs::rename(&temp_path, path)
    })();
    match result {
        Ok(()) => Ok(()),
        Err(error) => {
            let _ = fs::remove_file(&temp_path);
            Err(failure(
                StorageErrorCode::Io,
                format!("failed to publish JSONL storage {}", path.display()),
                Some(Arc::new(error)),
            ))
        }
    }
}

/// Checks cancellation before an owned blocking file operation starts.
///
/// # Errors
///
/// Returns `Aborted` if the context has been cancelled.
pub fn check_context(cx: &Context) -> Result<(), SessionError> {
    cx.check().map_err(|error| {
        failure(
            StorageErrorCode::Aborted,
            "operation cancelled",
            Some(Arc::new(error)),
        )
    })
}
