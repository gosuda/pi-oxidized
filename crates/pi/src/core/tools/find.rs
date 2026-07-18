//! Find files by glob pattern using `globset` + `ignore::WalkBuilder`.
//!
//! Ports `.references/pi/packages/coding-agent/src/core/tools/find.ts` without
//! spawning `fd`. Hidden entries are included, hierarchical `.gitignore` is
//! honored (with nested-repo boundary semantics), directories keep a trailing
//! `/`, paths are POSIX-relative and sorted deterministically.

use std::path::{Component, Path, PathBuf};
use std::sync::Arc;

use futures::FutureExt as _;
use futures::future::BoxFuture;
use globset::{GlobBuilder, GlobSetBuilder};
use ignore::WalkBuilder;
use pi_agent::{AgentTool, AgentToolResult, ToolError, ToolUpdates};
use pi_ai::ToolResultContent;
use pi_ai::types::TextContent;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value, json};
use tokio::task;
use tokio_util::sync::CancellationToken;

use super::{
    DEFAULT_MAX_BYTES, PathResolveError, TruncationOptions, TruncationResult, format_size,
    path_exists, resolve_to_cwd, truncate_head,
};

/// Default maximum number of results returned.
const DEFAULT_LIMIT: usize = 1000;

/// TypeBox-compatible find arguments (fixture `find.json`).
#[derive(Clone, Debug, Deserialize, JsonSchema, PartialEq, Serialize)]
pub struct FindToolInput {
    /// Glob pattern to match files, e.g. `*.ts`, `**/*.json`, or `src/**/*.spec.ts`.
    #[schemars(
        description = "Glob pattern to match files, e.g. '*.ts', '**/*.json', or 'src/**/*.spec.ts'"
    )]
    pub pattern: String,
    /// Directory to search in (default: current directory).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schemars(description = "Directory to search in (default: current directory)")]
    pub path: Option<String>,
    /// Maximum number of results (default: 1000).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schemars(description = "Maximum number of results (default: 1000)")]
    pub limit: Option<f64>,
}

/// Optional structured details returned by the find tool.
#[derive(Clone, Debug, Default, Deserialize, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct FindToolDetails {
    /// Truncation metadata when the 50 KiB head limit applied.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub truncation: Option<TruncationResult>,
    /// Effective result limit when that limit was hit.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub result_limit_reached: Option<usize>,
}

/// Options for [`FindTool`].
#[derive(Clone, Debug)]
pub struct FindToolOptions {
    /// Working directory used to resolve relative paths.
    pub cwd: PathBuf,
}

impl FindToolOptions {
    /// Builds options for `cwd`.
    #[must_use]
    pub fn new(cwd: impl Into<PathBuf>) -> Self {
        Self { cwd: cwd.into() }
    }
}

/// Agent tool that finds files by glob pattern.
#[derive(Clone, Debug)]
pub struct FindTool {
    cwd: PathBuf,
    parameters: Value,
    description: String,
}

impl FindTool {
    /// Creates a find tool rooted at `cwd`.
    #[must_use]
    pub fn new(cwd: impl Into<PathBuf>) -> Self {
        Self::with_options(FindToolOptions::new(cwd))
    }

    /// Creates a find tool from explicit options.
    #[must_use]
    pub fn with_options(options: FindToolOptions) -> Self {
        let description = format!(
            "Search for files by glob pattern. Returns matching file paths relative to the search directory. Respects .gitignore. Output is truncated to {DEFAULT_LIMIT} results or {}KB (whichever is hit first).",
            DEFAULT_MAX_BYTES / 1024
        );
        Self {
            cwd: options.cwd,
            parameters: find_parameters_schema(),
            description,
        }
    }

    /// Returns the JSON Schema for find arguments (normalized `TypeBox` shape).
    #[must_use]
    pub fn parameters_schema() -> Value {
        find_parameters_schema()
    }

    /// Validates raw tool arguments into [`FindToolInput`].
    ///
    /// # Errors
    ///
    /// Returns [`ToolError`] when required fields are missing or mistyped.
    pub fn parse_input(args: &Map<String, Value>) -> Result<FindToolInput, ToolError> {
        serde_json::from_value(Value::Object(args.clone()))
            .map_err(|error| ToolError::new(format!("Find tool input is invalid. {error}")))
    }
}

impl AgentTool for FindTool {
    fn name(&self) -> &'static str {
        "find"
    }

    fn label(&self) -> &'static str {
        "find"
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
            let input = FindTool::parse_input(&args)?;
            let path_arg = input.path.as_deref().unwrap_or(".");
            let search_path = resolve_to_cwd(path_arg, cwd.to_string_lossy().as_ref())
                .map_err(|error| path_error(&error))?;
            let effective_limit = effective_limit(input.limit, DEFAULT_LIMIT);
            throw_if_cancelled(&cancel)?;

            if !path_exists(&search_path).await {
                return Err(ToolError::new(format!("Path not found: {search_path}")));
            }
            throw_if_cancelled(&cancel)?;

            let pattern = input.pattern.clone();
            let search_root = PathBuf::from(&search_path);
            let cancel_for_walk = cancel.clone();
            let results = task::spawn_blocking(move || {
                collect_find_matches(&pattern, &search_root, effective_limit, &cancel_for_walk)
            })
            .await
            .map_err(|error| ToolError::new(format!("find walk failed: {error}")))??;
            throw_if_cancelled(&cancel)?;

            if results.is_empty() {
                return Ok(text_result("No files found matching pattern", None));
            }

            let result_limit_reached = results.len() >= effective_limit;
            let raw_output = results.join("\n");
            let truncation = truncate_head(
                &raw_output,
                TruncationOptions {
                    max_lines: Some(usize::MAX),
                    max_bytes: Some(DEFAULT_MAX_BYTES),
                },
            );
            let mut output = truncation.content.clone();
            let mut details = FindToolDetails::default();
            let mut notices = Vec::new();
            if result_limit_reached {
                notices.push(format!(
                    "{effective_limit} results limit reached. Use limit={} for more, or refine pattern",
                    effective_limit.saturating_mul(2)
                ));
                details.result_limit_reached = Some(effective_limit);
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

            let details =
                if details.result_limit_reached.is_some() || details.truncation.is_some() {
                    Some(details)
                } else {
                    None
                };
            Ok(text_result(output, details))
        }
        .boxed()
    }
}

fn collect_find_matches(
    pattern: &str,
    search_root: &Path,
    effective_limit: usize,
    cancel: &CancellationToken,
) -> Result<Vec<String>, ToolError> {
    throw_if_cancelled(cancel)?;

    let (matcher, match_full_path) = compile_find_glob(pattern)?;
    let require_git = is_inside_git_repo(search_root);

    let mut builder = WalkBuilder::new(search_root);
    builder
        .hidden(false)
        .follow_links(false)
        .git_ignore(true)
        .git_global(true)
        .git_exclude(true)
        .ignore(true)
        .parents(true)
        .require_git(require_git)
        .sort_by_file_path(std::cmp::Ord::cmp);

    let mut found = Vec::new();
    for entry in builder.build() {
        throw_if_cancelled(cancel)?;
        let Ok(entry) = entry else {
            continue;
        };
        if entry.depth() == 0 {
            continue;
        }
        let path = entry.path();
        let Ok(relative) = path.strip_prefix(search_root) else {
            continue;
        };
        if relative.as_os_str().is_empty() {
            continue;
        }

        let is_dir = entry.file_type().is_some_and(|ft| ft.is_dir());
        let is_hit = if match_full_path {
            let candidate = to_posix(relative);
            matcher.is_match(Path::new(&candidate))
                || (is_dir && matcher.is_match(Path::new(&format!("{candidate}/"))))
        } else {
            let name = entry.file_name().to_string_lossy();
            matcher.is_match(Path::new(name.as_ref()))
        };
        if !is_hit {
            continue;
        }

        let mut display = to_posix(relative);
        if is_dir && !display.ends_with('/') {
            display.push('/');
        }
        found.push(display);
        // Collect one extra hit so resultLimitReached is true when the limit
        // is hit; keep only effective_limit entries in the final list.
        if found.len() > effective_limit {
            found.truncate(effective_limit);
            break;
        }
    }

    // Deterministic sorted output (assignment); WalkBuilder sort covers
    // traversal order, but re-sort POSIX strings for stability across OSes.
    found.sort();
    if found.len() > effective_limit {
        found.truncate(effective_limit);
    }
    Ok(found)
}

fn compile_find_glob(pattern: &str) -> Result<(globset::GlobSet, bool), ToolError> {
    let mut effective = pattern.to_owned();
    let match_full_path = pattern.contains('/');
    if match_full_path
        && !pattern.starts_with('/')
        && !pattern.starts_with("**/")
        && pattern != "**"
    {
        effective = format!("**/{pattern}");
    }

    let glob = GlobBuilder::new(&effective)
        .literal_separator(false)
        .backslash_escape(true)
        .build()
        .map_err(|error| ToolError::new(format!("error parsing glob: {error}")))?;
    let mut set = GlobSetBuilder::new();
    set.add(glob);
    let matcher = set
        .build()
        .map_err(|error| ToolError::new(format!("error parsing glob: {error}")))?;
    Ok((matcher, match_full_path))
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

fn text_result(text: impl Into<String>, details: Option<FindToolDetails>) -> AgentToolResult {
    AgentToolResult {
        content: vec![ToolResultContent::Text(TextContent::new(text))],
        details: details_value(details),
        added_tool_names: None,
        terminate: None,
    }
}

fn details_value(details: Option<FindToolDetails>) -> Value {
    details.map_or(Value::Null, |details| {
        serde_json::to_value(details).unwrap_or_else(|_| json!({}))
    })
}

fn find_parameters_schema() -> Value {
    normalize_tool_schema(schemars::schema_for!(FindToolInput))
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

/// Builds an [`Arc<dyn AgentTool>`] find tool for `cwd`.
#[must_use]
pub fn create_find_tool(cwd: impl Into<PathBuf>) -> Arc<dyn AgentTool> {
    Arc::new(FindTool::new(cwd))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    use serde_json::json;
    use tempfile::tempdir;

    fn fixture_schema() -> Result<Value, serde_json::Error> {
        let text = include_str!(
            "../../../../../.agent-tasks/pi-rust-rewrite/fixtures/tool-schemas/find.json"
        );
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

    async fn run(tool: &FindTool, args: &Value) -> Result<AgentToolResult, ToolError> {
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
        assert_eq!(FindTool::parameters_schema(), fixture_schema()?);
        Ok(())
    }

    #[tokio::test]
    async fn includes_hidden_and_respects_gitignore() -> Result<(), Box<dyn std::error::Error>> {
        let dir = tempdir()?;
        fs::create_dir(dir.path().join(".secret"))?;
        fs::write(dir.path().join(".secret/hidden.txt"), "hidden")?;
        fs::write(dir.path().join("visible.txt"), "visible")?;
        fs::write(dir.path().join(".gitignore"), "ignored.txt\n")?;
        fs::write(dir.path().join("ignored.txt"), "ignored")?;
        fs::write(dir.path().join("kept.txt"), "kept")?;

        let tool = FindTool::new(dir.path());
        let text = text_of(
            &run(
                &tool,
                &json!({"pattern": "**/*.txt", "path": dir.path().to_string_lossy()}),
            )
            .await?,
        );
        let lines: Vec<&str> = text
            .lines()
            .map(str::trim)
            .filter(|line| !line.is_empty() && !line.starts_with('['))
            .collect();
        assert!(lines.contains(&"visible.txt"));
        assert!(lines.contains(&".secret/hidden.txt"));
        assert!(lines.contains(&"kept.txt"));
        assert!(!lines.contains(&"ignored.txt"));
        Ok(())
    }

    #[tokio::test]
    async fn path_glob_and_directory_suffix() -> Result<(), Box<dyn std::error::Error>> {
        let dir = tempdir()?;
        fs::create_dir_all(dir.path().join("src/nested"))?;
        fs::write(dir.path().join("src/nested/a.spec.ts"), "x")?;
        fs::write(dir.path().join("src/other.ts"), "y")?;
        fs::create_dir_all(dir.path().join("src/nested/dirmatch"))?;

        let tool = FindTool::new(dir.path());
        let text = text_of(
            &run(
                &tool,
                &json!({"pattern": "src/**/*.spec.ts", "path": dir.path().to_string_lossy()}),
            )
            .await?,
        );
        assert!(text.contains("src/nested/a.spec.ts"));
        assert!(!text.contains("src/other.ts"));

        let dirs = text_of(
            &run(
                &tool,
                &json!({"pattern": "**/dirmatch", "path": dir.path().to_string_lossy()}),
            )
            .await?,
        );
        assert!(dirs.lines().any(|line| line.ends_with("dirmatch/")));
        Ok(())
    }

    #[tokio::test]
    async fn result_limit_notice_and_sorted() -> Result<(), Box<dyn std::error::Error>> {
        let dir = tempdir()?;
        for name in ["c.txt", "a.txt", "b.txt"] {
            fs::write(dir.path().join(name), "x")?;
        }
        let tool = FindTool::new(dir.path());
        let result = run(
            &tool,
            &json!({"pattern": "*.txt", "path": dir.path().to_string_lossy(), "limit": 2}),
        )
        .await?;
        let text = text_of(&result);
        let body: Vec<&str> = text
            .lines()
            .filter(|line| !line.is_empty() && !line.starts_with('['))
            .collect();
        assert_eq!(body.len(), 2);
        assert!(body[0] <= body[1]);
        assert!(
            text.contains("[2 results limit reached. Use limit=4 for more, or refine pattern]")
        );
        Ok(())
    }

    #[tokio::test]
    async fn bad_glob_and_flag_pattern() -> Result<(), Box<dyn std::error::Error>> {
        let dir = tempdir()?;
        let tool = FindTool::new(dir.path());
        let result = run(
            &tool,
            &json!({"pattern": "[", "path": dir.path().to_string_lossy()}),
        )
        .await;
        let Err(err) = result else {
            return Err("invalid glob unexpectedly succeeded".into());
        };
        assert!(
            err.message().to_lowercase().contains("glob")
                || err.message().to_lowercase().contains("error"),
            "{}",
            err.message()
        );

        let text = text_of(
            &run(
                &tool,
                &json!({"pattern": "--help", "path": dir.path().to_string_lossy()}),
            )
            .await?,
        );
        assert_eq!(text, "No files found matching pattern");
        Ok(())
    }

    #[tokio::test]
    async fn missing_path_and_cancel() -> Result<(), Box<dyn std::error::Error>> {
        let dir = tempdir()?;
        let tool = FindTool::new(dir.path());
        let result = run(&tool, &json!({"pattern": "*", "path": "missing-dir"})).await;
        let Err(err) = result else {
            return Err("missing path unexpectedly succeeded".into());
        };
        assert!(err.message().starts_with("Path not found:"));

        let cancel = CancellationToken::new();
        cancel.cancel();
        let result = tool
            .execute(
                "t",
                json_map(&json!({"pattern": "*"}))?,
                cancel,
                ToolUpdates::noop(),
            )
            .await;
        let Err(err) = result else {
            return Err("cancelled find unexpectedly succeeded".into());
        };
        assert_eq!(err.message(), "Operation aborted");
        Ok(())
    }
}
