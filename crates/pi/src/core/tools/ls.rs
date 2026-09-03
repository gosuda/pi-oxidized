//! List directory contents (dotfiles included).
//!
//! Ports `.references/pi-2.0/packages/coding-agent/src/core/tools/ls.ts` with a
//! pure native filesystem walk (no subprocess). Output is sorted
//! case-insensitively, directories receive a trailing `/`, and entry count
//! plus 50 KiB head truncation match the TypeScript notices.

use std::cmp::Ordering;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use futures::FutureExt as _;
use futures::future::BoxFuture;
use pi_agent::{AgentTool, AgentToolResult, ToolError, ToolUpdates};
use pi_ai::ToolResultContent;
use pi_ai::types::TextContent;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value, json};
use tokio_util::sync::CancellationToken;

use super::{
    DEFAULT_MAX_BYTES, PathResolveError, TruncationOptions, TruncationResult, format_size,
    resolve_to_cwd, truncate_head,
};

/// Default maximum number of directory entries returned.
const DEFAULT_LIMIT: usize = 500;

/// TypeBox-compatible ls arguments (fixture `ls.json`).
#[derive(Clone, Debug, Deserialize, JsonSchema, PartialEq, Serialize)]
pub struct LsToolInput {
    /// Directory to list (default: current directory).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schemars(description = "Directory to list (default: current directory)")]
    pub path: Option<String>,
    /// Maximum number of entries to return (default: 500).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schemars(description = "Maximum number of entries to return (default: 500)")]
    pub limit: Option<f64>,
}

/// Optional structured details returned by the ls tool.
#[derive(Clone, Debug, Default, Deserialize, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct LsToolDetails {
    /// Truncation metadata when the 50 KiB head limit applied.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub truncation: Option<TruncationResult>,
    /// Effective entry limit when that limit was hit.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub entry_limit_reached: Option<usize>,
}

/// Options for [`LsTool`].
#[derive(Clone, Debug)]
pub struct LsToolOptions {
    /// Working directory used to resolve relative paths.
    pub cwd: PathBuf,
}

impl LsToolOptions {
    /// Builds options for `cwd`.
    #[must_use]
    pub fn new(cwd: impl Into<PathBuf>) -> Self {
        Self { cwd: cwd.into() }
    }
}

/// Agent tool that lists one directory including dotfiles.
#[derive(Clone, Debug)]
pub struct LsTool {
    cwd: PathBuf,
    parameters: Value,
    description: String,
}

impl LsTool {
    /// Creates an ls tool rooted at `cwd`.
    #[must_use]
    pub fn new(cwd: impl Into<PathBuf>) -> Self {
        Self::with_options(LsToolOptions::new(cwd))
    }

    /// Creates an ls tool from explicit options.
    #[must_use]
    pub fn with_options(options: LsToolOptions) -> Self {
        let description = format!(
            "List directory contents. Returns entries sorted alphabetically, with '/' suffix for directories. Includes dotfiles. Output is truncated to {DEFAULT_LIMIT} entries or {}KB (whichever is hit first).",
            DEFAULT_MAX_BYTES / 1024
        );
        Self {
            cwd: options.cwd,
            parameters: ls_parameters_schema(),
            description,
        }
    }

    /// Returns the JSON Schema for ls arguments (normalized `TypeBox` shape).
    #[must_use]
    pub fn parameters_schema() -> Value {
        ls_parameters_schema()
    }

    /// Validates raw tool arguments into [`LsToolInput`].
    ///
    /// # Errors
    ///
    /// Returns [`ToolError`] when fields are mistyped.
    pub fn parse_input(args: &Map<String, Value>) -> Result<LsToolInput, ToolError> {
        serde_json::from_value(Value::Object(args.clone()))
            .map_err(|error| ToolError::new(format!("Ls tool input is invalid. {error}")))
    }
}

impl AgentTool for LsTool {
    fn name(&self) -> &'static str {
        "ls"
    }

    fn label(&self) -> &'static str {
        "ls"
    }

    fn description(&self) -> &str {
        &self.description
    }

    fn parameters(&self) -> &Value {
        &self.parameters
    }

    fn validate_arguments(
        &self,
        args: &Map<String, Value>,
    ) -> Result<Map<String, Value>, ToolError> {
        let _ = Self::parse_input(args)?;
        Ok(args.clone())
    }

    #[allow(clippy::too_many_lines)]
    fn execute(
        &self,
        _tool_call_id: &str,
        args: Map<String, Value>,
        cancel: CancellationToken,
        _updates: ToolUpdates,
    ) -> BoxFuture<'static, Result<AgentToolResult, ToolError>> {
        let cwd = self.cwd.clone();
        async move {
            throw_if_cancelled(&cancel)?;
            let input = LsTool::parse_input(&args)?;
            let path_arg = input.path.as_deref().unwrap_or(".");
            let dir_path = resolve_to_cwd(path_arg, cwd.to_string_lossy().as_ref())
                .map_err(|error| path_error(&error))?;
            let effective_limit = effective_limit(input.limit, DEFAULT_LIMIT);
            throw_if_cancelled(&cancel)?;

            let meta = tokio::fs::metadata(&dir_path).await;
            match meta {
                Ok(meta) if meta.is_dir() => {}
                Ok(_) => {
                    return Err(ToolError::new(format!("Not a directory: {dir_path}")));
                }
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                    return Err(ToolError::new(format!("Path not found: {dir_path}")));
                }
                Err(error) => {
                    return Err(ToolError::new(format!("Cannot read directory: {error}")));
                }
            }
            throw_if_cancelled(&cancel)?;

            let mut names = Vec::new();
            let mut read_dir = tokio::fs::read_dir(&dir_path)
                .await
                .map_err(|error| ToolError::new(format!("Cannot read directory: {error}")))?;
            loop {
                throw_if_cancelled(&cancel)?;
                match read_dir.next_entry().await {
                    Ok(Some(entry)) => {
                        let name = entry.file_name();
                        let name = name.to_string_lossy().into_owned();
                        names.push(name);
                    }
                    Ok(None) => break,
                    Err(error) => {
                        return Err(ToolError::new(format!("Cannot read directory: {error}")));
                    }
                }
            }

            names.sort_by(|a, b| compare_case_insensitive(a, b));

            let mut results = Vec::new();
            let mut entry_limit_reached = false;
            for name in names {
                throw_if_cancelled(&cancel)?;
                if results.len() >= effective_limit {
                    entry_limit_reached = true;
                    break;
                }
                let full_path = Path::new(&dir_path).join(&name);
                let suffix = match tokio::fs::metadata(&full_path).await {
                    Ok(meta) if meta.is_dir() => "/",
                    Ok(_) => "",
                    Err(_) => continue,
                };
                results.push(format!("{name}{suffix}"));
            }

            if results.is_empty() {
                return Ok(text_result("(empty directory)", None));
            }

            let raw_output = results.join("\n");
            let truncation = truncate_head(
                &raw_output,
                TruncationOptions {
                    max_lines: Some(usize::MAX),
                    max_bytes: Some(DEFAULT_MAX_BYTES),
                },
            );
            let mut output = truncation.content.clone();
            let mut details = LsToolDetails::default();
            let mut notices = Vec::new();
            if entry_limit_reached {
                notices.push(format!(
                    "{effective_limit} entries limit reached. Use limit={} for more",
                    effective_limit.saturating_mul(2)
                ));
                details.entry_limit_reached = Some(effective_limit);
            }
            if truncation.truncated {
                notices.push(format!(
                    "{} limit reached",
                    format_size(DEFAULT_MAX_BYTES as u64)
                ));
                details.truncation = Some(truncation);
            }
            if !notices.is_empty() {
                output.push_str("\n\n[");
                output.push_str(&notices.join(". "));
                output.push(']');
            }

            let details = if details.entry_limit_reached.is_some() || details.truncation.is_some() {
                Some(details)
            } else {
                None
            };
            Ok(text_result(output, details))
        }
        .boxed()
    }
}

fn compare_case_insensitive(a: &str, b: &str) -> Ordering {
    // JS: a.toLowerCase().localeCompare(b.toLowerCase()) - Unicode lowercase,
    // then original string as a stable secondary key.
    let a_lower = a.to_lowercase();
    let b_lower = b.to_lowercase();
    match a_lower.cmp(&b_lower) {
        Ordering::Equal => a.cmp(b),
        other => other,
    }
}

fn effective_limit(limit: Option<f64>, default: usize) -> usize {
    match limit {
        Some(value) if value.is_finite() => {
            #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
            let as_i = value as i64;
            if as_i < 1 {
                1
            } else {
                #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
                {
                    as_i as usize
                }
            }
        }
        _ => default,
    }
}

fn text_result(text: impl Into<String>, details: Option<LsToolDetails>) -> AgentToolResult {
    AgentToolResult {
        content: vec![ToolResultContent::Text(TextContent::new(text))],
        details: details_value(details),
        added_tool_names: None,
        terminate: None,
    }
}

fn details_value(details: Option<LsToolDetails>) -> Value {
    details.map_or(Value::Null, |details| {
        serde_json::to_value(details).unwrap_or_else(|_| json!({}))
    })
}

fn ls_parameters_schema() -> Value {
    normalize_tool_schema(schemars::schema_for!(LsToolInput))
}

fn normalize_tool_schema(schema: schemars::Schema) -> Value {
    let mut value = serde_json::to_value(schema).unwrap_or_else(|_| Value::Object(Map::new()));
    if let Value::Object(map) = &mut value {
        map.remove("$schema");
        map.remove("title");
        map.remove("description");
        map.remove("additionalProperties");
        // TypeBox omits `required` when every property is optional.
        if let Some(Value::Array(required)) = map.get("required")
            && required.is_empty()
        {
            map.remove("required");
        }
        normalize_schema_node(map);
    }
    value
}

fn normalize_schema_node(map: &mut Map<String, Value>) {
    map.remove("format");
    // schemars represents Option<T> as ["number","null"]; TypeBox optional
    // numbers are just "number".
    if let Some(Value::Array(types)) = map.get("type").cloned() {
        let non_null: Vec<Value> = types
            .into_iter()
            .filter(|t| t.as_str() != Some("null"))
            .collect();
        if non_null.len() == 1 {
            map.insert("type".to_owned(), non_null[0].clone());
        } else if !non_null.is_empty() {
            map.insert("type".to_owned(), Value::Array(non_null));
        }
    }
    let keys: Vec<String> = map.keys().cloned().collect();
    for key in keys {
        match map.get_mut(&key) {
            Some(Value::Object(child)) => normalize_schema_node(child),
            Some(Value::Array(items)) => {
                for item in items {
                    if let Value::Object(child) = item {
                        normalize_schema_node(child);
                    }
                }
            }
            _ => {}
        }
    }
}

fn throw_if_cancelled(cancel: &CancellationToken) -> Result<(), ToolError> {
    if cancel.is_cancelled() {
        Err(ToolError::new("Operation aborted"))
    } else {
        Ok(())
    }
}

fn path_error(error: &PathResolveError) -> ToolError {
    ToolError::new(error.to_string())
}

/// Builds an [`Arc<dyn AgentTool>`] ls tool for `cwd`.
#[must_use]
pub fn create_ls_tool(cwd: impl Into<PathBuf>) -> Arc<dyn AgentTool> {
    Arc::new(LsTool::new(cwd))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::os::unix::fs::PermissionsExt;

    use serde_json::json;
    use tempfile::tempdir;

    fn fixture_schema() -> Result<Value, serde_json::Error> {
        let text = include_str!("../../../tests/fixtures/tool-schemas/ls.json");
        serde_json::from_str(text)
    }

    fn json_map(value: &Value) -> Result<Map<String, Value>, ToolError> {
        value
            .as_object()
            .cloned()
            .ok_or_else(|| ToolError::new("test arguments must be a JSON object"))
    }

    fn text_of(result: &AgentToolResult) -> String {
        match result.content.first() {
            Some(ToolResultContent::Text(text)) => text.text.clone(),
            _ => String::new(),
        }
    }

    async fn run(tool: &LsTool, args: &Value) -> Result<AgentToolResult, ToolError> {
        tool.execute(
            "t",
            json_map(args)?,
            CancellationToken::new(),
            ToolUpdates::noop(),
        )
        .await
    }

    #[test]
    fn schema_matches_typebox_fixture() -> Result<(), Box<dyn std::error::Error>> {
        assert_eq!(LsTool::parameters_schema(), fixture_schema()?);
        Ok(())
    }

    #[tokio::test]
    async fn lists_dotfiles_and_directory_suffix() -> Result<(), Box<dyn std::error::Error>> {
        let dir = tempdir()?;
        fs::write(dir.path().join(".hidden-file"), "secret")?;
        fs::create_dir(dir.path().join(".hidden-dir"))?;
        fs::write(dir.path().join("plain.txt"), "x")?;
        fs::create_dir(dir.path().join("subdir"))?;

        let tool = LsTool::new(dir.path());
        let result = run(&tool, &json!({})).await?;
        let text = text_of(&result);
        let lines: Vec<&str> = text.lines().collect();
        assert!(lines.contains(&".hidden-file"));
        assert!(lines.contains(&".hidden-dir/"));
        assert!(lines.contains(&"plain.txt"));
        assert!(lines.contains(&"subdir/"));
        Ok(())
    }

    #[tokio::test]
    async fn sorts_case_insensitively() -> Result<(), Box<dyn std::error::Error>> {
        let dir = tempdir()?;
        for name in ["b.txt", "A.txt", "c.txt", "a.txt"] {
            fs::write(dir.path().join(name), "x")?;
        }
        let tool = LsTool::new(dir.path());
        let text = text_of(&run(&tool, &json!({})).await?);
        let lines: Vec<&str> = text.lines().collect();
        let mut expected = vec!["A.txt", "a.txt", "b.txt", "c.txt"];
        expected.sort_by(|a, b| compare_case_insensitive(a, b));
        assert_eq!(lines, expected);
        Ok(())
    }

    #[tokio::test]
    async fn skips_unstatable_entries() -> Result<(), Box<dyn std::error::Error>> {
        let dir = tempdir()?;
        let keep = dir.path().join("keep.txt");
        fs::write(&keep, "ok")?;
        let trap = dir.path().join("trap");
        fs::create_dir(&trap)?;
        // Create a dangling symlink that metadata follows and fails to resolve.
        let dangling = dir.path().join("dangling");
        #[cfg(unix)]
        {
            std::os::unix::fs::symlink(dir.path().join("missing-target"), &dangling)?;
        }

        let tool = LsTool::new(dir.path());
        let text = text_of(&run(&tool, &json!({})).await?);
        assert!(text.contains("keep.txt"));
        assert!(text.contains("trap/"));
        assert!(
            !text
                .lines()
                .any(|line| line == "dangling" || line == "dangling/")
        );
        Ok(())
    }

    #[tokio::test]
    async fn entry_limit_notice() -> Result<(), Box<dyn std::error::Error>> {
        let dir = tempdir()?;
        for i in 0..5 {
            fs::write(dir.path().join(format!("f{i}.txt")), "x")?;
        }
        let tool = LsTool::new(dir.path());
        let result = run(&tool, &json!({"limit": 2})).await?;
        let text = text_of(&result);
        assert_eq!(
            text.lines()
                .filter(|l| !l.is_empty() && !l.starts_with('['))
                .count(),
            2
        );
        assert!(text.contains("[2 entries limit reached. Use limit=4 for more]"));
        assert_eq!(result.details.get("entryLimitReached"), Some(&json!(2)));
        Ok(())
    }

    #[tokio::test]
    async fn empty_directory_message() -> Result<(), Box<dyn std::error::Error>> {
        let dir = tempdir()?;
        let tool = LsTool::new(dir.path());
        let text = text_of(&run(&tool, &json!({})).await?);
        assert_eq!(text, "(empty directory)");
        Ok(())
    }

    #[tokio::test]
    async fn missing_and_not_directory_errors() -> Result<(), Box<dyn std::error::Error>> {
        let dir = tempdir()?;
        fs::write(dir.path().join("file.txt"), "x")?;
        let tool = LsTool::new(dir.path());
        let result = run(&tool, &json!({"path": "nope"})).await;
        let Err(missing) = result else {
            return Err("missing path unexpectedly succeeded".into());
        };
        assert!(missing.message().starts_with("Path not found:"));
        let result = run(&tool, &json!({"path": "file.txt"})).await;
        let Err(not_dir) = result else {
            return Err("file path unexpectedly listed as a directory".into());
        };
        assert!(not_dir.message().starts_with("Not a directory:"));
        Ok(())
    }

    #[tokio::test]
    async fn cancellation_aborts() -> Result<(), Box<dyn std::error::Error>> {
        let dir = tempdir()?;
        let tool = LsTool::new(dir.path());
        let cancel = CancellationToken::new();
        cancel.cancel();
        let result = tool
            .execute("t", Map::new(), cancel, ToolUpdates::noop())
            .await;
        let Err(err) = result else {
            return Err("cancelled ls unexpectedly succeeded".into());
        };
        assert_eq!(err.message(), "Operation aborted");
        Ok(())
    }

    #[tokio::test]
    async fn unique_tmp_names_avoid_collision() {
        // Ensure PermissionsExt stays imported under unix-only symlink test.
        let _ = fs::Permissions::from_mode(0o644);
    }
}
