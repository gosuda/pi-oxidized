//! Strict JSONL loading with deliberately narrow nondeterminism normalization.

use std::{
    fs::File,
    io::{BufRead, BufReader},
    path::{Path, PathBuf},
};

use serde_json::Value;
use thiserror::Error;

/// Errors produced while loading a provider golden transcript.
#[derive(Debug, Error)]
pub enum GoldenError {
    /// The fixture could not be opened.
    #[error("failed to open JSONL golden {path}: {source}")]
    Open {
        /// Fixture path.
        path: PathBuf,
        /// Filesystem error.
        #[source]
        source: std::io::Error,
    },
    /// A line could not be read as UTF-8 text.
    #[error("failed to read line {line} from JSONL golden {path}: {source}")]
    Read {
        /// Fixture path.
        path: PathBuf,
        /// One-based line number.
        line: usize,
        /// Filesystem or UTF-8 error.
        #[source]
        source: std::io::Error,
    },
    /// A nonempty line was not one complete JSON value.
    #[error("invalid JSON on line {line} of golden {path}: {source}")]
    Json {
        /// Fixture path.
        path: PathBuf,
        /// One-based line number.
        line: usize,
        /// JSON parser error.
        #[source]
        source: serde_json::Error,
    },
}

/// Loads nonempty JSONL records without changing any field.
pub fn load_jsonl(path: impl AsRef<Path>) -> Result<Vec<Value>, GoldenError> {
    let path = path.as_ref();
    let file = File::open(path).map_err(|source| GoldenError::Open {
        path: path.to_path_buf(),
        source,
    })?;
    let mut records = Vec::new();
    for (index, line) in BufReader::new(file).lines().enumerate() {
        let line_number = index.saturating_add(1);
        let line = line.map_err(|source| GoldenError::Read {
            path: path.to_path_buf(),
            line: line_number,
            source,
        })?;
        if line.trim().is_empty() {
            continue;
        }
        let record = serde_json::from_str(&line).map_err(|source| GoldenError::Json {
            path: path.to_path_buf(),
            line: line_number,
            source,
        })?;
        records.push(record);
    }
    Ok(records)
}

/// Loads JSONL records and removes fields named exactly `timestamp` at every depth.
///
/// No other likely-nondeterministic field is touched: IDs, usage, stop reasons, request
/// metadata, errors, and similarly named fields such as `createdAt` remain byte-for-byte
/// represented in their parsed JSON values.
pub fn load_normalized_jsonl(path: impl AsRef<Path>) -> Result<Vec<Value>, GoldenError> {
    let mut records = load_jsonl(path)?;
    for record in &mut records {
        normalize_timestamps(record);
    }
    Ok(records)
}

/// Recursively removes object members named exactly `timestamp`.
pub fn normalize_timestamps(value: &mut Value) {
    match value {
        Value::Array(values) => {
            for value in values {
                normalize_timestamps(value);
            }
        }
        Value::Object(object) => {
            object.remove("timestamp");
            for value in object.values_mut() {
                normalize_timestamps(value);
            }
        }
        Value::Null | Value::Bool(_) | Value::Number(_) | Value::String(_) => {}
    }
}

#[cfg(test)]
mod tests {
    use std::{
        fs,
        sync::atomic::{AtomicU64, Ordering},
    };

    use serde_json::json;

    use super::*;

    static NEXT_FIXTURE: AtomicU64 = AtomicU64::new(0);

    fn fixture_path(label: &str) -> PathBuf {
        let sequence = NEXT_FIXTURE.fetch_add(1, Ordering::Relaxed);
        std::env::temp_dir().join(format!(
            "pi-ai-golden-{}-{label}-{sequence}.jsonl",
            std::process::id()
        ))
    }

    #[test]
    fn invalid_jsonl_reports_path_and_one_based_line() -> Result<(), Box<dyn std::error::Error>> {
        let path = fixture_path("invalid");
        fs::write(&path, "{\"ok\":true}\n{not-json}\n")?;
        let result = load_jsonl(&path);
        fs::remove_file(&path)?;

        let Err(error) = result else {
            return Err("invalid JSONL unexpectedly loaded".into());
        };
        assert!(matches!(error, GoldenError::Json { line: 2, .. }));
        let message = error.to_string();
        assert!(message.contains(&path.display().to_string()));
        assert!(message.contains("line 2"));
        Ok(())
    }

    #[test]
    fn normalization_removes_only_exact_timestamp_fields_recursively() {
        let mut value = json!({
            "timestamp": 123,
            "id": "response-123",
            "usage": { "timestamp": "variable", "input": 7 },
            "events": [
                { "timestamp": 456, "stopReason": "stop" },
                { "nested": { "timestamp": null, "error": "exact" } }
            ],
            "createdAt": "keep",
            "eventTimestamp": "keep",
            "timestamp_ms": 789,
            "body": "keep"
        });
        normalize_timestamps(&mut value);

        assert_eq!(
            value,
            json!({
                "id": "response-123",
                "usage": { "input": 7 },
                "events": [
                    { "stopReason": "stop" },
                    { "nested": { "error": "exact" } }
                ],
                "createdAt": "keep",
                "eventTimestamp": "keep",
                "timestamp_ms": 789,
                "body": "keep"
            })
        );
    }

    #[test]
    fn raw_loader_preserves_timestamp_and_blank_lines_are_ignored()
    -> Result<(), Box<dyn std::error::Error>> {
        let path = fixture_path("raw");
        fs::write(&path, "\n{\"timestamp\":1,\"id\":\"a\"}\n  \n")?;
        let records = load_jsonl(&path)?;
        let normalized = load_normalized_jsonl(&path)?;
        fs::remove_file(&path)?;
        assert_eq!(records, vec![json!({"timestamp": 1, "id": "a"})]);
        assert_eq!(normalized, vec![json!({"id": "a"})]);
        Ok(())
    }
}
