//! Search file contents with native regex / literal matching.
//!
//! Ports `.references/pi/packages/coding-agent/src/core/tools/grep.ts` without
//! spawning `rg`. Directory searches use `ignore::WalkBuilder` (hidden +
//! hierarchical gitignore). Output lines are 1-indexed, match/context/unread
//! formats match TypeScript, lines are truncated to 500 chars, and match count
//! plus 50 KiB head truncation produce the exact notices.

use std::path::{Component, Path, PathBuf};
use std::sync::Arc;

use futures::FutureExt as _;
use futures::future::BoxFuture;
use globset::{GlobBuilder, GlobSet, GlobSetBuilder};
use ignore::WalkBuilder;
use memchr::memmem;
use pi_agent::{AgentTool, AgentToolResult, ToolError, ToolUpdates};
use pi_ai::ToolResultContent;
use pi_ai::types::TextContent;
use regex::RegexBuilder;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value, json};
use tokio::task;
use tokio_util::sync::CancellationToken;

use super::{
    DEFAULT_MAX_BYTES, GREP_MAX_LINE_LENGTH, PathResolveError, TruncationOptions, TruncationResult,
    format_size, resolve_to_cwd, truncate_head, truncate_line,
};

/// Default maximum number of matches returned.
const DEFAULT_LIMIT: usize = 100;

/// TypeBox-compatible grep arguments (fixture `grep.json`).
#[derive(Clone, Debug, Deserialize, JsonSchema, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct GrepToolInput {
    /// Search pattern (regex or literal string).
    #[schemars(description = "Search pattern (regex or literal string)")]
    pub pattern: String,
    /// Directory or file to search (default: current directory).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schemars(description = "Directory or file to search (default: current directory)")]
    pub path: Option<String>,
    /// Filter files by glob pattern, e.g. `*.ts` or `**/*.spec.ts`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schemars(description = "Filter files by glob pattern, e.g. '*.ts' or '**/*.spec.ts'")]
    pub glob: Option<String>,
    /// Case-insensitive search (default: false). Omitted and `false` are
    /// strictly case-sensitive; only `true` enables case-insensitive search.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schemars(description = "Case-insensitive search (default: false)")]
    pub ignore_case: Option<bool>,
    /// Treat pattern as literal string instead of regex (default: false).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schemars(description = "Treat pattern as literal string instead of regex (default: false)")]
    pub literal: Option<bool>,
    /// Number of lines to show before and after each match (default: 0).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schemars(description = "Number of lines to show before and after each match (default: 0)")]
    pub context: Option<f64>,
    /// Maximum number of matches to return (default: 100).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schemars(description = "Maximum number of matches to return (default: 100)")]
    pub limit: Option<f64>,
}

/// Optional structured details returned by the grep tool.
#[derive(Clone, Debug, Default, Deserialize, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct GrepToolDetails {
    /// Truncation metadata when the 50 KiB head limit applied.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub truncation: Option<TruncationResult>,
    /// Effective match limit when that limit was hit.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub match_limit_reached: Option<usize>,
    /// Whether any line was truncated to [`GREP_MAX_LINE_LENGTH`] chars.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub lines_truncated: Option<bool>,
}

/// Options for [`GrepTool`].
#[derive(Clone, Debug)]
pub struct GrepToolOptions {
    /// Working directory used to resolve relative paths.
    pub cwd: PathBuf,
}

impl GrepToolOptions {
    /// Builds options for `cwd`.
    #[must_use]
    pub fn new(cwd: impl Into<PathBuf>) -> Self {
        Self { cwd: cwd.into() }
    }
}

/// Agent tool that searches file contents.
#[derive(Clone, Debug)]
pub struct GrepTool {
    cwd: PathBuf,
    parameters: Value,
    description: String,
}

impl GrepTool {
    /// Creates a grep tool rooted at `cwd`.
    #[must_use]
    pub fn new(cwd: impl Into<PathBuf>) -> Self {
        Self::with_options(GrepToolOptions::new(cwd))
    }

    /// Creates a grep tool from explicit options.
    #[must_use]
    pub fn with_options(options: GrepToolOptions) -> Self {
        let description = format!(
            "Search file contents for a pattern. Returns matching lines with file paths and line numbers. Respects .gitignore. Output is truncated to {DEFAULT_LIMIT} matches or {}KB (whichever is hit first). Long lines are truncated to {GREP_MAX_LINE_LENGTH} chars.",
            DEFAULT_MAX_BYTES / 1024
        );
        Self {
            cwd: options.cwd,
            parameters: grep_parameters_schema(),
            description,
        }
    }

    /// Returns the JSON Schema for grep arguments (normalized `TypeBox` shape).
    #[must_use]
    pub fn parameters_schema() -> Value {
        grep_parameters_schema()
    }

    /// Validates raw tool arguments into [`GrepToolInput`].
    ///
    /// # Errors
    ///
    /// Returns [`ToolError`] when required fields are missing or mistyped.
    pub fn parse_input(args: &Map<String, Value>) -> Result<GrepToolInput, ToolError> {
        serde_json::from_value(Value::Object(args.clone()))
            .map_err(|error| ToolError::new(format!("Grep tool input is invalid. {error}")))
    }
}

impl AgentTool for GrepTool {
    fn name(&self) -> &'static str {
        "grep"
    }

    fn label(&self) -> &'static str {
        "grep"
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
            let input = GrepTool::parse_input(&args)?;
            let path_arg = input.path.as_deref().unwrap_or(".");
            let search_path = resolve_to_cwd(path_arg, cwd.to_string_lossy().as_ref())
                .map_err(|error| path_error(&error))?;
            let effective_limit = effective_limit_at_least_one(input.limit, DEFAULT_LIMIT);
            let context_value = input.context.map_or(0, |value| {
                if value.is_finite() && value > 0.0 {
                    #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
                    {
                        value as usize
                    }
                } else {
                    0
                }
            });
            let literal = input.literal.unwrap_or(false);
            let ignore_case = input.ignore_case.unwrap_or(false);
            throw_if_cancelled(&cancel)?;

            let meta = tokio::fs::metadata(&search_path).await;
            let is_directory = match meta {
                Ok(meta) => meta.is_dir(),
                Err(_) => {
                    return Err(ToolError::new(format!("Path not found: {search_path}")));
                }
            };
            throw_if_cancelled(&cancel)?;

            let pattern = input.pattern.clone();
            let glob = input.glob.clone();
            let search_root = PathBuf::from(&search_path);
            let cancel_for_search = cancel.clone();
            let search_result = task::spawn_blocking(move || {
                run_grep_search(
                    &pattern,
                    &search_root,
                    is_directory,
                    glob.as_deref(),
                    ignore_case,
                    literal,
                    context_value,
                    effective_limit,
                    &cancel_for_search,
                )
            })
            .await
            .map_err(|error| ToolError::new(format!("grep search failed: {error}")))??;
            throw_if_cancelled(&cancel)?;

            if search_result.output_lines.is_empty() {
                return Ok(text_result("No matches found", None));
            }

            let raw_output = search_result.output_lines.join("\n");
            let truncation = truncate_head(
                &raw_output,
                TruncationOptions {
                    max_lines: Some(usize::MAX),
                    max_bytes: Some(DEFAULT_MAX_BYTES),
                },
            );
            let mut output = truncation.content.clone();
            let mut details = GrepToolDetails::default();
            let mut notices = Vec::new();
            if search_result.match_limit_reached {
                notices.push(format!(
                    "{effective_limit} matches limit reached. Use limit={} for more, or refine pattern",
                    effective_limit.saturating_mul(2)
                ));
                details.match_limit_reached = Some(effective_limit);
            }
            if truncation.truncated {
                notices.push(format!(
                    "{} limit reached",
                    format_size(DEFAULT_MAX_BYTES as u64)
                ));
                details.truncation = Some(truncation);
            }
            if search_result.lines_truncated {
                notices.push(format!(
                    "Some lines truncated to {GREP_MAX_LINE_LENGTH} chars. Use read tool to see full lines"
                ));
                details.lines_truncated = Some(true);
            }
            if !notices.is_empty() {
                output.push_str("\n\n[");
                output.push_str(&notices.join(". "));
                output.push(']');
            }

            let details = if details.match_limit_reached.is_some()
                || details.truncation.is_some()
                || details.lines_truncated.is_some()
            {
                Some(details)
            } else {
                None
            };
            Ok(text_result(output, details))
        }
        .boxed()
    }
}

struct GrepSearchResult {
    output_lines: Vec<String>,
    match_limit_reached: bool,
    lines_truncated: bool,
}

enum Matcher {
    Regex(regex::Regex),
    Literal { needle: Vec<u8>, ignore_case: bool },
}

impl Matcher {
    fn is_match(&self, line: &str) -> bool {
        match self {
            Self::Regex(re) => re.is_match(line),
            Self::Literal {
                needle,
                ignore_case,
            } => {
                if *ignore_case {
                    let hay = line.to_lowercase();
                    let needle = String::from_utf8_lossy(needle).to_lowercase();
                    hay.contains(&needle)
                } else {
                    memmem::find(line.as_bytes(), needle).is_some()
                }
            }
        }
    }
}

#[allow(clippy::too_many_arguments, clippy::too_many_lines)]
fn run_grep_search(
    pattern: &str,
    search_root: &Path,
    is_directory: bool,
    glob: Option<&str>,
    ignore_case: bool,
    literal: bool,
    context_value: usize,
    effective_limit: usize,
    cancel: &CancellationToken,
) -> Result<GrepSearchResult, ToolError> {
    throw_if_cancelled(cancel)?;
    let matcher = compile_matcher(pattern, ignore_case, literal)?;
    let glob_filter = match glob {
        Some(pattern) => Some(compile_file_glob(pattern)?),
        None => None,
    };

    let mut found_hits: Vec<(PathBuf, usize, Option<String>)> = Vec::new();
    let mut match_limit_reached = false;

    if is_directory {
        let mut builder = WalkBuilder::new(search_root);
        builder
            .hidden(false)
            .follow_links(false)
            .git_ignore(true)
            .git_global(true)
            .git_exclude(true)
            .ignore(true)
            .parents(true)
            .require_git(is_inside_git_repo(search_root))
            .sort_by_file_path(std::cmp::Ord::cmp);

        for entry in builder.build() {
            throw_if_cancelled(cancel)?;
            if match_limit_reached {
                break;
            }
            let Ok(entry) = entry else {
                continue;
            };
            let path = entry.path();
            if entry.file_type().is_some_and(|ft| ft.is_dir()) {
                continue;
            }
            if let Some(filter) = &glob_filter {
                let rel = path.strip_prefix(search_root).unwrap_or(path).to_path_buf();
                let candidate = to_posix(&rel);
                let name = path
                    .file_name()
                    .map(|n| n.to_string_lossy().into_owned())
                    .unwrap_or_default();
                if !filter.is_match(Path::new(&candidate)) && !filter.is_match(Path::new(&name)) {
                    continue;
                }
            }
            search_file(
                path,
                &matcher,
                effective_limit,
                &mut found_hits,
                &mut match_limit_reached,
                cancel,
            )?;
        }
    } else {
        if let Some(filter) = &glob_filter {
            let name = search_root
                .file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_default();
            if !filter.is_match(Path::new(&name)) {
                return Ok(GrepSearchResult {
                    output_lines: Vec::new(),
                    match_limit_reached: false,
                    lines_truncated: false,
                });
            }
        }
        search_file(
            search_root,
            &matcher,
            effective_limit,
            &mut found_hits,
            &mut match_limit_reached,
            cancel,
        )?;
    }

    let mut output_lines = Vec::new();
    let mut lines_truncated = false;
    let mut file_cache: std::collections::HashMap<PathBuf, Option<Vec<String>>> =
        std::collections::HashMap::new();

    for (file_path, line_number, line_text) in found_hits {
        throw_if_cancelled(cancel)?;
        let relative = format_path(&file_path, search_root, is_directory);
        if context_value == 0
            && let Some(text) = line_text
        {
            let sanitized = sanitize_match_line(&text);
            let truncated = truncate_line(&sanitized);
            if truncated.was_truncated {
                lines_truncated = true;
            }
            output_lines.push(format!("{relative}:{line_number}: {}", truncated.text));
            continue;
        }

        let lines = file_cache
            .entry(file_path.clone())
            .or_insert_with(|| read_file_lines(&file_path));
        match lines {
            None => {
                output_lines.push(format!("{relative}:{line_number}: (unable to read file)"));
            }
            Some(lines) if lines.is_empty() => {
                output_lines.push(format!("{relative}:{line_number}: (unable to read file)"));
            }
            Some(lines) => {
                let start = if context_value > 0 {
                    line_number.saturating_sub(context_value).max(1)
                } else {
                    line_number
                };
                let end = if context_value > 0 {
                    (line_number + context_value).min(lines.len())
                } else {
                    line_number
                };
                for current in start..=end {
                    let line_text = lines.get(current - 1).map_or("", String::as_str);
                    let sanitized = line_text.replace('\r', "");
                    let truncated = truncate_line(&sanitized);
                    if truncated.was_truncated {
                        lines_truncated = true;
                    }
                    if current == line_number {
                        output_lines.push(format!("{relative}:{current}: {}", truncated.text));
                    } else {
                        output_lines.push(format!("{relative}-{current}- {}", truncated.text));
                    }
                }
            }
        }
    }

    Ok(GrepSearchResult {
        output_lines,
        match_limit_reached,
        lines_truncated,
    })
}

fn search_file(
    path: &Path,
    matcher: &Matcher,
    effective_limit: usize,
    found_hits: &mut Vec<(PathBuf, usize, Option<String>)>,
    match_limit_reached: &mut bool,
    cancel: &CancellationToken,
) -> Result<(), ToolError> {
    throw_if_cancelled(cancel)?;
    let Ok(bytes) = std::fs::read(path) else {
        return Ok(());
    };
    // Skip obvious binary files (NUL byte in first 8 KiB).
    let probe = &bytes[..bytes.len().min(8192)];
    if probe.contains(&0) {
        return Ok(());
    }
    let content = String::from_utf8_lossy(&bytes);
    let normalized = content.replace("\r\n", "\n").replace('\r', "\n");
    for (index, line) in normalized.split('\n').enumerate() {
        throw_if_cancelled(cancel)?;
        if found_hits.len() >= effective_limit {
            *match_limit_reached = true;
            break;
        }
        if matcher.is_match(line) {
            found_hits.push((path.to_path_buf(), index + 1, Some(line.to_owned())));
            if found_hits.len() >= effective_limit {
                *match_limit_reached = true;
                break;
            }
        }
    }
    Ok(())
}

fn read_file_lines(path: &Path) -> Option<Vec<String>> {
    let bytes = std::fs::read(path).ok()?;
    let content = String::from_utf8_lossy(&bytes);
    let normalized = content.replace("\r\n", "\n").replace('\r', "\n");
    Some(normalized.split('\n').map(str::to_owned).collect())
}

fn sanitize_match_line(text: &str) -> String {
    let mut sanitized = text.replace("\r\n", "\n").replace('\r', "");
    if sanitized.ends_with('\n') {
        sanitized.pop();
    }
    sanitized
}

fn compile_matcher(pattern: &str, ignore_case: bool, literal: bool) -> Result<Matcher, ToolError> {
    if literal {
        Ok(Matcher::Literal {
            needle: pattern.as_bytes().to_vec(),
            ignore_case,
        })
    } else {
        let re = RegexBuilder::new(pattern)
            .case_insensitive(ignore_case)
            .multi_line(false)
            .build()
            .map_err(|error| ToolError::new(format!("Invalid regular expression: {error}")))?;
        Ok(Matcher::Regex(re))
    }
}

fn compile_file_glob(pattern: &str) -> Result<GlobSet, ToolError> {
    let glob = GlobBuilder::new(pattern)
        .literal_separator(false)
        .backslash_escape(true)
        .build()
        .map_err(|error| ToolError::new(format!("error parsing glob: {error}")))?;
    let mut set = GlobSetBuilder::new();
    set.add(glob);
    set.build()
        .map_err(|error| ToolError::new(format!("error parsing glob: {error}")))
}

fn format_path(file_path: &Path, search_root: &Path, is_directory: bool) -> String {
    if is_directory && let Ok(relative) = file_path.strip_prefix(search_root) {
        let posix = to_posix(relative);
        if !posix.is_empty() && !posix.starts_with("..") {
            return posix;
        }
    }
    file_path.file_name().map_or_else(
        || file_path.to_string_lossy().into_owned(),
        |n| n.to_string_lossy().into_owned(),
    )
}

fn is_inside_git_repo(start: &Path) -> bool {
    let mut current = start.to_path_buf();
    loop {
        if current.join(".git").exists() {
            return true;
        }
        if !current.pop() {
            return false;
        }
    }
}

fn to_posix(path: &Path) -> String {
    let mut out = String::new();
    for component in path.components() {
        match component {
            Component::Normal(part) => {
                if !out.is_empty() {
                    out.push('/');
                }
                out.push_str(&part.to_string_lossy());
            }
            Component::ParentDir => {
                if !out.is_empty() {
                    out.push('/');
                }
                out.push_str("..");
            }
            Component::CurDir | Component::RootDir | Component::Prefix(_) => {}
        }
    }
    if out.is_empty() {
        path.to_string_lossy().replace('\\', "/")
    } else {
        out
    }
}
fn effective_limit_at_least_one(limit: Option<f64>, default: usize) -> usize {
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
        _ => default.max(1),
    }
}

fn text_result(text: impl Into<String>, details: Option<GrepToolDetails>) -> AgentToolResult {
    AgentToolResult {
        content: vec![ToolResultContent::Text(TextContent::new(text))],
        details: details_value(details),
        added_tool_names: None,
        terminate: None,
    }
}

fn details_value(details: Option<GrepToolDetails>) -> Value {
    details.map_or(Value::Null, |details| {
        serde_json::to_value(details).unwrap_or_else(|_| json!({}))
    })
}

fn grep_parameters_schema() -> Value {
    normalize_tool_schema(schemars::schema_for!(GrepToolInput))
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

/// Builds an [`Arc<dyn AgentTool>`] grep tool for `cwd`.
#[must_use]
pub fn create_grep_tool(cwd: impl Into<PathBuf>) -> Arc<dyn AgentTool> {
    Arc::new(GrepTool::new(cwd))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    use serde_json::json;
    use tempfile::tempdir;

    fn fixture_schema() -> Result<Value, serde_json::Error> {
        let text = include_str!("../../../tests/fixtures/tool-schemas/grep.json");
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

    async fn run(tool: &GrepTool, args: &Value) -> Result<AgentToolResult, ToolError> {
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
        assert_eq!(GrepTool::parameters_schema(), fixture_schema()?);
        Ok(())
    }

    #[tokio::test]
    async fn omitted_ignore_case_is_case_sensitive() -> Result<(), Box<dyn std::error::Error>> {
        let dir = tempdir()?;
        fs::write(dir.path().join("case.txt"), "Foo\nfoo\n")?;
        let tool = GrepTool::new(dir.path());

        let omitted = text_of(
            &run(
                &tool,
                &json!({"pattern": "foo", "path": dir.path().join("case.txt").to_string_lossy()}),
            )
            .await?,
        );
        assert!(omitted.contains("case.txt:2: foo"));
        assert!(!omitted.contains("Foo"));

        let insensitive = text_of(
            &run(
                &tool,
                &json!({
                    "pattern": "foo",
                    "path": dir.path().join("case.txt").to_string_lossy(),
                    "ignoreCase": true
                }),
            )
            .await?,
        );
        assert!(insensitive.contains("case.txt:1: Foo"));
        assert!(insensitive.contains("case.txt:2: foo"));
        Ok(())
    }

    #[tokio::test]
    async fn single_file_match_format() -> Result<(), Box<dyn std::error::Error>> {
        let dir = tempdir()?;
        let file = dir.path().join("example.txt");
        fs::write(&file, "first line\nmatch line\nlast line")?;
        let tool = GrepTool::new(dir.path());
        let text = text_of(
            &run(
                &tool,
                &json!({"pattern": "match", "path": file.to_string_lossy()}),
            )
            .await?,
        );
        assert!(text.contains("example.txt:2: match line"));
        Ok(())
    }

    #[tokio::test]
    async fn context_limit_and_notice() -> Result<(), Box<dyn std::error::Error>> {
        let dir = tempdir()?;
        let file = dir.path().join("context.txt");
        fs::write(
            &file,
            "before\nmatch one\nafter\nmiddle\nmatch two\nafter two",
        )?;
        let tool = GrepTool::new(dir.path());
        let text = text_of(
            &run(
                &tool,
                &json!({
                    "pattern": "match",
                    "path": file.to_string_lossy(),
                    "limit": 1,
                    "context": 1
                }),
            )
            .await?,
        );
        assert!(text.contains("context.txt-1- before"));
        assert!(text.contains("context.txt:2: match one"));
        assert!(text.contains("context.txt-3- after"));
        assert!(
            text.contains("[1 matches limit reached. Use limit=2 for more, or refine pattern]")
        );
        assert!(!text.contains("match two"));
        Ok(())
    }

    #[tokio::test]
    async fn ignore_case_literal_glob_and_unread() -> Result<(), Box<dyn std::error::Error>> {
        let dir = tempdir()?;
        fs::write(dir.path().join("a.ts"), "Hello world\nhello again\n")?;
        fs::write(dir.path().join("b.js"), "Hello nowhere\n")?;
        fs::write(dir.path().join("bin.dat"), b"Hello\0binary")?;
        fs::write(dir.path().join("case.txt"), "Foo\nfoo\n")?;

        let tool = GrepTool::new(dir.path());

        // Omitted ignoreCase is case-sensitive: "foo" does not match "Foo".
        let omitted = text_of(
            &run(
                &tool,
                &json!({"pattern": "foo", "path": dir.path().join("case.txt").to_string_lossy()}),
            )
            .await?,
        );
        assert!(omitted.contains("case.txt:2: foo"));
        assert!(!omitted.contains("Foo"));

        // Explicit true is case-insensitive.
        let insensitive = text_of(
            &run(
                &tool,
                &json!({
                    "pattern": "foo",
                    "path": dir.path().join("case.txt").to_string_lossy(),
                    "ignoreCase": true
                }),
            )
            .await?,
        );
        assert!(insensitive.contains("case.txt:1: Foo"));
        assert!(insensitive.contains("case.txt:2: foo"));

        // Default case-sensitive with glob filter.
        let sens = text_of(
            &run(
                &tool,
                &json!({
                    "pattern": "hello",
                    "path": dir.path().to_string_lossy(),
                    "glob": "*.ts"
                }),
            )
            .await?,
        );
        assert!(sens.contains("hello again"));
        assert!(!sens.contains("Hello world"));
        assert!(!sens.contains("b.js"));

        // Literal treats regex metacharacters as text.
        fs::write(dir.path().join("lit.txt"), "a+b\n")?;
        let lit = text_of(
            &run(
                &tool,
                &json!({
                    "pattern": "a+b",
                    "path": dir.path().join("lit.txt").to_string_lossy(),
                    "literal": true
                }),
            )
            .await?,
        );
        assert!(lit.contains("lit.txt:1: a+b"));

        // Flag-like patterns are not executed.
        let flag = text_of(
            &run(
                &tool,
                &json!({"pattern": "--pre=/tmp/x", "path": dir.path().to_string_lossy()}),
            )
            .await?,
        );
        assert_eq!(flag, "No matches found");
        Ok(())
    }

    #[tokio::test]
    async fn gitignore_hidden_and_line_truncation() -> Result<(), Box<dyn std::error::Error>> {
        let dir = tempdir()?;
        fs::write(dir.path().join(".gitignore"), "skip.txt\n")?;
        fs::write(dir.path().join("skip.txt"), "needle\n")?;
        fs::create_dir(dir.path().join(".hidden"))?;
        fs::write(dir.path().join(".hidden/h.txt"), "needle\n")?;
        fs::write(
            dir.path().join("keep.txt"),
            format!("{}\n", "x".repeat(600)),
        )?;
        fs::write(
            dir.path().join("keep.txt"),
            format!("needle {}\n", "x".repeat(600)),
        )?;

        let tool = GrepTool::new(dir.path());
        let text = text_of(
            &run(
                &tool,
                &json!({"pattern": "needle", "path": dir.path().to_string_lossy()}),
            )
            .await?,
        );
        assert!(!text.contains("skip.txt"));
        assert!(text.contains(".hidden/h.txt:1: needle"));
        assert!(text.contains("... [truncated]"));
        assert!(text.contains(&format!(
            "Some lines truncated to {GREP_MAX_LINE_LENGTH} chars. Use read tool to see full lines"
        )));
        Ok(())
    }

    #[tokio::test]
    async fn missing_path_and_cancel() -> Result<(), Box<dyn std::error::Error>> {
        let dir = tempdir()?;
        let tool = GrepTool::new(dir.path());
        let result = run(&tool, &json!({"pattern": "x", "path": "nope"})).await;
        let Err(err) = result else {
            return Err("missing path unexpectedly succeeded".into());
        };
        assert!(err.message().starts_with("Path not found:"));

        let cancel = CancellationToken::new();
        cancel.cancel();
        let result = tool
            .execute(
                "t",
                json_map(&json!({"pattern": "x"}))?,
                cancel,
                ToolUpdates::noop(),
            )
            .await;
        let Err(err) = result else {
            return Err("cancelled grep unexpectedly succeeded".into());
        };
        assert_eq!(err.message(), "Operation aborted");
        Ok(())
    }
}
