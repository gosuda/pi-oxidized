//! `/debug` support dump: rendered TUI lines plus session messages, written to
//! `{agent}/pi-debug.log`.
//!
//! Ports the observable shape of `handleDebugCommand` in the TypeScript
//! `modes/interactive/interactive-mode.ts`: an ISO timestamp, terminal size,
//! the fully rendered lines with their visible widths, and the agent messages
//! as JSONL. Two additions over the reference, both required by the rewrite:
//!
//! - **Redaction.** The dump is written to the user's disk and frequently
//!   shared for support, so credential-shaped substrings (bearer tokens,
//!   `Authorization` headers, `sk-` style keys, JSON `api_key` fields, and
//!   `x-api-key` headers) are masked before the file is written. Image data
//!   and ordinary text are preserved.
//! - **Atomic write.** The file is written to a sibling temporary path and
//!   renamed into place, so a crash mid-write never leaves a half dump.

use std::fmt::Write as _;
use std::io;
use std::path::{Path, PathBuf};

use regex::Regex;

use crate::core::config::{get_debug_log_path, get_debug_log_path_with};

/// Input captured for a debug dump.
#[derive(Debug)]
pub struct DebugDumpInput<'a> {
    /// ISO-8601 timestamp (UTC).
    pub timestamp: &'a str,
    /// Terminal width in columns.
    pub width: u16,
    /// Terminal height in rows.
    pub height: u16,
    /// Fully rendered lines paired with their visible (display) width.
    pub rendered_lines: &'a [(String, u16)],
    /// Agent messages serialized as JSONL, one entry per line.
    pub messages: &'a [String],
}

/// Render a debug dump to its full text form, with secrets redacted.
///
/// The section order and headers match the TypeScript reference so support
/// tooling can parse either implementation's output identically.
#[must_use]
pub fn render_debug_dump(input: &DebugDumpInput<'_>) -> String {
    let mut out = String::new();
    out.push_str(input.timestamp);
    out.push('\n');
    let _ = writeln!(out, "Terminal: {}x{}", input.width, input.height);
    let _ = writeln!(out, "Total lines: {}", input.rendered_lines.len());
    out.push_str("\n=== All rendered lines with visible widths ===\n");
    for (idx, (line, width)) in input.rendered_lines.iter().enumerate() {
        let redacted = redact_secrets(line);
        let _ = writeln!(out, "[{idx}] (w={width}) {redacted}");
    }
    out.push_str("\n=== Agent messages (JSONL) ===\n");
    for message in input.messages {
        let redacted = redact_secrets(message);
        out.push_str(&redacted);
        out.push('\n');
    }
    out
}

/// Mask credential-shaped substrings in `text`.
///
/// Recognizes:
/// - `Authorization: <scheme> <token>` and bare `Bearer <token>`
/// - `x-api-key: <value>`
/// - OpenAI-style `sk-…` keys (20+ word characters after the prefix)
/// - JSON fields named `apiKey` / `api_key` / `apikey` with a string value
///
/// Returns the text with each matched secret replaced by `[REDACTED]`, leaving
/// the surrounding key or scheme intact so the structure stays readable.
#[must_use]
pub fn redact_secrets(text: &str) -> String {
    let Ok(patterns) = redaction_patterns() else {
        return "[REDACTED]".to_owned();
    };
    let mut value = text.to_owned();
    for (regex, template) in patterns {
        value = regex.replace_all(&value, template).into_owned();
    }
    value
}

/// Write `content` to `path` atomically (temp file + rename).
///
/// Creates parent directories. The temporary file is a sibling of the target
/// so the rename stays on the same filesystem.
///
/// # Errors
///
/// Returns directory creation, write, or rename failures.
pub fn write_debug_dump_atomically(path: &Path, content: &str) -> io::Result<()> {
    if let Some(parent) = path.parent()
        && !parent.as_os_str().is_empty()
    {
        std::fs::create_dir_all(parent)?;
    }
    let temp_path = sibling_temp_path(path);
    std::fs::write(&temp_path, content)?;
    std::fs::rename(&temp_path, path).or_else(|_| {
        // Rename can fail across some filesystems; fall back to a plain write
        // so the dump still lands. Best-effort cleanup of the temp file.
        let _ = std::fs::remove_file(&temp_path);
        std::fs::write(path, content)
    })
}

/// Render and atomically write a dump to the host debug log path.
///
/// Returns the path written.
///
/// # Errors
///
/// Propagates [`write_debug_dump_atomically`] failures.
pub fn write_debug_dump(input: &DebugDumpInput<'_>) -> io::Result<PathBuf> {
    let path = get_debug_log_path();
    let rendered = render_debug_dump(input);
    write_debug_dump_atomically(&path, &rendered)?;
    Ok(path)
}

/// Render and atomically write a dump to the path resolved from `agent_dir`.
///
/// # Errors
///
/// Propagates [`write_debug_dump_atomically`] failures.
pub fn write_debug_dump_with(input: &DebugDumpInput<'_>, agent_dir: &Path) -> io::Result<PathBuf> {
    let path = get_debug_log_path_with(agent_dir);
    let rendered = render_debug_dump(input);
    write_debug_dump_atomically(&path, &rendered)?;
    Ok(path)
}

/// Build a sibling temporary path that does not collide with the target.
fn sibling_temp_path(path: &Path) -> PathBuf {
    let pid = std::process::id();
    let mut name = path.file_name().map_or_else(
        || "pi-debug.log".to_owned(),
        |n| n.to_string_lossy().into_owned(),
    );
    let _ = write!(name, ".tmp.{pid}");
    path.with_file_name(name)
}

/// Compiled redaction patterns paired with their replacement templates.
///
/// Each `$1` captures the key/scheme/header name so only the secret value is
/// stripped. Built per call because `/debug` is invoked manually and rarely.
fn redaction_patterns() -> Result<Vec<(Regex, String)>, regex::Error> {
    [
        (
            r"(?i)(authorization\s*:\s*)(?:bearer|basic|token|digest)?\s*\S+",
            "$1[REDACTED]",
        ),
        (r"(?i)(bearer\s+)\S+", "$1[REDACTED]"),
        (r"(?i)(x-api-key\s*:\s*)\S+", "$1[REDACTED]"),
        (r"(sk-)[A-Za-z0-9_-]{20,}", "$1[REDACTED]"),
        (r#"(?i)("(?:api[_-]?key)"\s*:\s*")[^"]*"#, "$1[REDACTED]\""),
    ]
    .into_iter()
    .map(|(pattern, replacement)| Regex::new(pattern).map(|regex| (regex, replacement.to_owned())))
    .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    type TestResult = Result<(), Box<dyn std::error::Error>>;

    fn sample_input<'a>(lines: &'a [(String, u16)], messages: &'a [String]) -> DebugDumpInput<'a> {
        DebugDumpInput {
            timestamp: "2026-07-18T00:00:00Z",
            width: 80,
            height: 24,
            rendered_lines: lines,
            messages,
        }
    }

    #[test]
    fn render_includes_sections_and_counts() {
        let lines = vec![("hello".to_owned(), 5u16), ("world".to_owned(), 5)];
        let messages = vec!["{\"role\":\"user\"}".to_owned()];
        let out = render_debug_dump(&sample_input(&lines, &messages));
        assert!(out.starts_with("2026-07-18T00:00:00Z\n"));
        assert!(out.contains("Terminal: 80x24"));
        assert!(out.contains("Total lines: 2"));
        assert!(out.contains("=== All rendered lines with visible widths ==="));
        assert!(out.contains("[0] (w=5) hello"));
        assert!(out.contains("=== Agent messages (JSONL) ==="));
        assert!(out.contains("{\"role\":\"user\"}"));
    }

    #[test]
    fn redact_masks_authorization_and_bearer() {
        // Construct the secret programmatically so no credential literal is
        // ever committed to source.
        let secret = format!("tok{}", "a".repeat(30));
        let header = format!("Authorization: Bearer {secret}");
        let redacted = redact_secrets(&header);
        assert!(!redacted.contains(&secret), "got: {redacted}");
        assert!(redacted.contains("Authorization:"));
        assert!(redacted.contains("[REDACTED]"));
        let bare = format!("token bearer {secret} end");
        let redacted = redact_secrets(&bare);
        assert!(!redacted.contains(&secret), "got: {redacted}");
    }

    #[test]
    fn redact_masks_sk_prefix_key() {
        let body = format!("key sk-{}", "b".repeat(40));
        let redacted = redact_secrets(&body);
        assert!(redacted.contains("sk-"));
        assert!(redacted.contains("[REDACTED]"));
        assert!(!redacted.contains(&"b".repeat(40)));
    }

    #[test]
    fn redact_masks_json_api_key_field() {
        let value = format!("value{}", "c".repeat(20));
        let json = format!("{{\"apiKey\":\"{value}\",\"n\":1}}");
        let redacted = redact_secrets(&json);
        assert!(redacted.contains("\"apiKey\":\"[REDACTED]\""));
        assert!(!redacted.contains(&value));
        // Non-secret fields are preserved.
        assert!(redacted.contains("\"n\":1"));
    }

    #[test]
    fn redact_leaves_plain_text_untouched() {
        let text = "The quick brown fox jumps over the lazy dog.";
        assert_eq!(redact_secrets(text), text);
    }

    #[test]
    fn atomic_write_creates_file_and_parents() -> TestResult {
        let dir = tempfile::tempdir()?;
        let target = dir.path().join("nested").join("pi-debug.log");
        write_debug_dump_atomically(&target, "body")?;
        assert_eq!(std::fs::read_to_string(&target)?, "body");
        // No leftover temp files.
        let parent = target
            .parent()
            .ok_or_else(|| std::io::Error::other("debug dump target has no parent"))?;
        let entries = std::fs::read_dir(parent)?.count();
        assert_eq!(entries, 1);
        Ok(())
    }

    #[test]
    fn atomic_write_overwrites_existing() -> TestResult {
        let dir = tempfile::tempdir()?;
        let target = dir.path().join("pi-debug.log");
        std::fs::write(&target, "old")?;
        write_debug_dump_atomically(&target, "new")?;
        assert_eq!(std::fs::read_to_string(&target)?, "new");
        Ok(())
    }

    #[test]
    fn write_to_agent_dir_uses_config_path() -> TestResult {
        let dir = tempfile::tempdir()?;
        let lines = vec![("hi".to_owned(), 2u16)];
        let input = sample_input(&lines, &[]);
        let path = write_debug_dump_with(&input, dir.path())?;
        assert_eq!(path, dir.path().join("pi-debug.log"));
        let content = std::fs::read_to_string(&path)?;
        assert!(content.contains("Terminal: 80x24"));
        Ok(())
    }
}
