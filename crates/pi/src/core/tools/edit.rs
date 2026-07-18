//! Edit tool: exact multi-replacement file edits under the mutation queue.
//!
//! Ports `.references/pi/packages/coding-agent/src/core/tools/edit.ts`.
//! Argument preparation folds legacy top-level `oldText`/`newText` and
//! stringified `edits` JSON. Execution serializes per-file mutations, preserves
//! BOM/CRLF, and applies original-coordinate non-overlapping replacements.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use futures::FutureExt as _;
use futures::future::BoxFuture;
use pi_agent::{AgentTool, AgentToolResult, ToolError, ToolUpdates};
use pi_ai::ToolResultContent;
use pi_ai::types::TextContent;
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};
use tokio_util::sync::CancellationToken;

use super::edit_diff::{
    Edit, apply_edits_to_normalized_content, detect_line_ending, generate_diff_string,
    generate_unified_patch, normalize_to_lf, restore_line_endings, strip_bom,
};
use super::{MutationQueueError, PathResolveError, resolve_to_cwd, with_file_mutation_queue};

/// One replacement entry in the public edit schema.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ReplaceEditInput {
    /// Exact text for one targeted replacement.
    pub old_text: String,
    /// Replacement text for this targeted edit.
    pub new_text: String,
}

/// TypeBox-compatible edit arguments (fixture `edit.json`).
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
pub struct EditToolInput {
    /// Path to the file to edit (relative or absolute).
    pub path: String,
    /// One or more targeted replacements.
    pub edits: Vec<ReplaceEditInput>,
}

/// Structured details returned by a successful edit.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct EditToolDetails {
    /// Display-oriented numbered diff.
    pub diff: String,
    /// Standard unified patch.
    pub patch: String,
    /// First changed line in the new file (1-based), when present.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub first_changed_line: Option<usize>,
}

/// Options for [`EditTool`].
#[derive(Clone, Debug)]
pub struct EditToolOptions {
    /// Working directory used to resolve relative paths.
    pub cwd: PathBuf,
}

impl EditToolOptions {
    /// Builds options for `cwd`.
    #[must_use]
    pub fn new(cwd: impl Into<PathBuf>) -> Self {
        Self { cwd: cwd.into() }
    }
}

/// Agent tool that applies unique non-overlapping text replacements.
#[derive(Clone, Debug)]
pub struct EditTool {
    cwd: PathBuf,
    parameters: Value,
    description: String,
}

impl EditTool {
    /// Creates an edit tool rooted at `cwd`.
    #[must_use]
    pub fn new(cwd: impl Into<PathBuf>) -> Self {
        Self::with_options(EditToolOptions::new(cwd))
    }

    /// Creates an edit tool from explicit options.
    #[must_use]
    pub fn with_options(options: EditToolOptions) -> Self {
        Self {
            cwd: options.cwd,
            parameters: edit_parameters_schema(),
            description: "Edit a single file using exact text replacement. Every edits[].oldText must match a unique, non-overlapping region of the original file. If two changes affect the same block or nearby lines, merge them into one edit instead of emitting overlapping edits. Do not include large unchanged regions just to connect distant changes.".to_owned(),
        }
    }

    /// Returns the JSON Schema for edit arguments (normalized `TypeBox` shape).
    #[must_use]
    pub fn parameters_schema() -> Value {
        edit_parameters_schema()
    }

    /// Compatibility shim for legacy top-level oldText/newText and stringified edits.
    #[must_use]
    pub fn prepare_edit_arguments(raw: &Map<String, Value>) -> Map<String, Value> {
        let mut args = raw.clone();

        if let Some(Value::String(edits_str)) = args.get("edits").cloned()
            && let Ok(parsed) = serde_json::from_str::<Value>(&edits_str)
            && parsed.is_array()
        {
            args.insert("edits".to_owned(), parsed);
        }

        let old_text = args.get("oldText").cloned();
        let new_text = args.get("newText").cloned();
        if let (Some(Value::String(old)), Some(Value::String(new))) = (old_text, new_text) {
            let mut edits = match args.get("edits") {
                Some(Value::Array(items)) => items.clone(),
                _ => Vec::new(),
            };
            edits.push(serde_json::json!({
                "oldText": old,
                "newText": new,
            }));
            args.insert("edits".to_owned(), Value::Array(edits));
            args.remove("oldText");
            args.remove("newText");
        }

        args
    }

    /// Validates prepared arguments and requires a non-empty edits list.
    ///
    /// # Errors
    ///
    /// Returns [`ToolError`] when required fields are missing, mistyped, or
    /// `edits` is empty.
    pub fn parse_input(args: &Map<String, Value>) -> Result<EditToolInput, ToolError> {
        let input: EditToolInput = serde_json::from_value(Value::Object(args.clone()))
            .map_err(|error| ToolError::new(format!("Edit tool input is invalid. {error}")))?;
        if input.edits.is_empty() {
            return Err(ToolError::new(
                "Edit tool input is invalid. edits must contain at least one replacement.",
            ));
        }
        Ok(input)
    }

    /// Formats the success text using the caller's path string.
    #[must_use]
    pub fn success_text(path: &str, edit_count: usize) -> String {
        format!("Successfully replaced {edit_count} block(s) in {path}.")
    }
}

impl AgentTool for EditTool {
    fn name(&self) -> &'static str {
        "edit"
    }

    fn label(&self) -> &'static str {
        "edit"
    }

    fn description(&self) -> &str {
        &self.description
    }

    fn parameters(&self) -> &Value {
        &self.parameters
    }

    fn prepare_arguments(&self, raw: &Map<String, Value>) -> Result<Map<String, Value>, ToolError> {
        Ok(Self::prepare_edit_arguments(raw))
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
            let edit = parse_edit_request(&cwd, &args)?;
            apply_edit(edit, &cancel).await
        }
        .boxed()
    }
}

struct PreparedEdit {
    absolute: PathBuf,
    path_for_message: String,
    edits: Vec<Edit>,
}

fn parse_edit_request(cwd: &Path, args: &Map<String, Value>) -> Result<PreparedEdit, ToolError> {
    let input = EditTool::parse_input(args)?;
    let absolute_path = resolve_to_cwd(&input.path, cwd.to_string_lossy().as_ref())
        .map_err(|error| path_error(&error))?;
    let edits = input
        .edits
        .into_iter()
        .map(|edit| Edit {
            old_text: edit.old_text,
            new_text: edit.new_text,
        })
        .collect();
    Ok(PreparedEdit {
        absolute: PathBuf::from(absolute_path),
        path_for_message: input.path,
        edits,
    })
}

async fn apply_edit(
    edit: PreparedEdit,
    cancel: &CancellationToken,
) -> Result<AgentToolResult, ToolError> {
    let cancel_for_queue = cancel.clone();
    let result = with_file_mutation_queue(&edit.absolute, || {
        let absolute = edit.absolute.clone();
        let path_for_message = edit.path_for_message.clone();
        let edits = edit.edits.clone();
        let cancel = cancel_for_queue.clone();
        async move { apply_edit_mutation(&absolute, &path_for_message, &edits, &cancel).await }
    })
    .await
    .map_err(|error| mutation_error(&error))??;

    throw_if_cancelled(cancel)?;
    Ok(result)
}

async fn apply_edit_mutation(
    absolute: &Path,
    path_for_message: &str,
    edits: &[Edit],
    cancel: &CancellationToken,
) -> Result<AgentToolResult, ToolError> {
    throw_if_cancelled(cancel)?;

    // access R_OK | W_OK
    let metadata = tokio::fs::metadata(absolute)
        .await
        .map_err(|error| access_error(path_for_message, &error))?;
    if metadata.permissions().readonly() {
        return Err(ToolError::new(format!(
            "Could not edit file: {path_for_message}. Error code: EACCES."
        )));
    }
    // Also need readable: try open
    let _ = tokio::fs::File::open(absolute)
        .await
        .map_err(|error| access_error(path_for_message, &error))?;
    throw_if_cancelled(cancel)?;

    let bytes = tokio::fs::read(absolute)
        .await
        .map_err(|error| access_error(path_for_message, &error))?;
    let raw_content = String::from_utf8(bytes).map_err(|_| {
        ToolError::new(format!(
            "Could not edit file: {path_for_message}. File is not valid UTF-8."
        ))
    })?;
    throw_if_cancelled(cancel)?;

    let (bom, content) = strip_bom(&raw_content);
    let original_ending = detect_line_ending(&content);
    let normalized_content = normalize_to_lf(&content);
    let applied = apply_edits_to_normalized_content(&normalized_content, edits, path_for_message)
        .map_err(ToolError::new)?;
    throw_if_cancelled(cancel)?;

    let final_content = bom + &restore_line_endings(&applied.new_content, original_ending);
    tokio::fs::write(absolute, final_content.as_bytes())
        .await
        .map_err(|error| {
            ToolError::new(format!("Could not edit file: {path_for_message}. {error}."))
        })?;
    throw_if_cancelled(cancel)?;

    let diff_result = generate_diff_string(&applied.base_content, &applied.new_content, 4);
    let patch = generate_unified_patch(
        path_for_message,
        &applied.base_content,
        &applied.new_content,
        4,
    );
    let details = EditToolDetails {
        diff: diff_result.diff,
        patch,
        first_changed_line: diff_result.first_changed_line,
    };
    let details_value = serde_json::to_value(details)
        .map_err(|error| ToolError::new(format!("Could not serialize edit details: {error}")))?;

    Ok(AgentToolResult {
        content: vec![ToolResultContent::Text(TextContent::new(
            EditTool::success_text(path_for_message, edits.len()),
        ))],
        details: details_value,
        added_tool_names: None,
        terminate: None,
    })
}

fn edit_parameters_schema() -> Value {
    serde_json::json!({
        "type": "object",
        "required": ["path", "edits"],
        "properties": {
            "path": {
                "type": "string",
                "description": "Path to the file to edit (relative or absolute)"
            },
            "edits": {
                "type": "array",
                "description": "One or more targeted replacements. Each edit is matched against the original file, not incrementally. Do not include overlapping or nested edits. If two changes touch the same block or nearby lines, merge them into one edit instead.",
                "items": {
                    "type": "object",
                    "required": ["oldText", "newText"],
                    "properties": {
                        "oldText": {
                            "type": "string",
                            "description": "Exact text for one targeted replacement. It must be unique in the original file and must not overlap with any other edits[].oldText in the same call."
                        },
                        "newText": {
                            "type": "string",
                            "description": "Replacement text for this targeted edit."
                        }
                    }
                }
            }
        }
    })
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

fn mutation_error(error: &MutationQueueError) -> ToolError {
    ToolError::new(error.to_string())
}

fn access_error(path: &str, error: &std::io::Error) -> ToolError {
    let message = match error.raw_os_error() {
        Some(_) => {
            let code = error.kind();
            // Prefer errno-style names when available via Display of ErrorKind is not ENOENT.
            // Use io::Error::to_string and map common kinds.
            let code_str = match code {
                std::io::ErrorKind::NotFound => "ENOENT",
                std::io::ErrorKind::PermissionDenied => "EACCES",
                std::io::ErrorKind::IsADirectory => "EISDIR",
                std::io::ErrorKind::NotADirectory => "ENOTDIR",
                _ => {
                    // Fall back to full error string for unknown codes.
                    return ToolError::new(format!("Could not edit file: {path}. Error: {error}."));
                }
            };
            format!("Error code: {code_str}")
        }
        None => format!("Error: {error}"),
    };
    ToolError::new(format!("Could not edit file: {path}. {message}."))
}

/// Builds an [`Arc<dyn AgentTool>`] edit tool for `cwd`.
#[must_use]
pub fn create_edit_tool(cwd: impl Into<PathBuf>) -> Arc<dyn AgentTool> {
    Arc::new(EditTool::new(cwd))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;
    use std::sync::Arc;
    use std::time::Duration;

    use serde_json::json;
    use tempfile::tempdir;
    use tokio::sync::Barrier;

    fn fixture_schema() -> Result<Value, serde_json::Error> {
        let text = include_str!(
            "../../../../../.agent-tasks/pi-rust-rewrite/fixtures/tool-schemas/edit.json"
        );
        serde_json::from_str(text)
    }

    fn json_map(value: &Value) -> Map<String, Value> {
        assert!(value.is_object(), "test input must be a JSON object");
        value.as_object().cloned().unwrap_or_default()
    }

    fn text_of(result: &AgentToolResult) -> String {
        match result.content.first() {
            Some(ToolResultContent::Text(text)) => text.text.clone(),
            _ => String::new(),
        }
    }

    #[test]
    fn schema_matches_typebox_fixture() -> Result<(), Box<dyn std::error::Error>> {
        let schema = EditTool::parameters_schema();
        assert_eq!(schema, fixture_schema()?);
        Ok(())
    }

    #[test]
    fn prepare_folds_legacy_old_new_text() {
        let prepared = EditTool::prepare_edit_arguments(&json_map(&json!({
            "path": "file.txt",
            "oldText": "before",
            "newText": "after"
        })));
        assert_eq!(
            prepared,
            json_map(&json!({
                "path": "file.txt",
                "edits": [{"oldText": "before", "newText": "after"}]
            }))
        );
        assert!(!prepared.contains_key("oldText"));
        assert!(!prepared.contains_key("newText"));
    }

    #[test]
    fn prepare_appends_legacy_to_existing_edits() {
        let prepared = EditTool::prepare_edit_arguments(&json_map(&json!({
            "path": "file.txt",
            "edits": [{"oldText": "a", "newText": "b"}],
            "oldText": "c",
            "newText": "d"
        })));
        assert_eq!(
            prepared.get("edits"),
            Some(&json!([
                {"oldText": "a", "newText": "b"},
                {"oldText": "c", "newText": "d"}
            ]))
        );
    }

    #[test]
    fn prepare_parses_stringified_edits() {
        let prepared = EditTool::prepare_edit_arguments(&json_map(&json!({
            "path": "file.txt",
            "edits": "[{\"oldText\":\"a\",\"newText\":\"b\"}]"
        })));
        assert_eq!(
            prepared.get("edits"),
            Some(&json!([{"oldText":"a","newText":"b"}]))
        );
    }

    #[test]
    fn empty_edits_rejected() -> Result<(), Box<dyn std::error::Error>> {
        let err = EditTool::parse_input(&json_map(&json!({
            "path": "f.txt",
            "edits": []
        })))
        .err()
        .ok_or("empty edits were accepted")?;
        assert!(
            err.message()
                .contains("edits must contain at least one replacement")
        );
        Ok(())
    }

    #[tokio::test]
    async fn exact_single_and_multi_edits() -> Result<(), Box<dyn std::error::Error>> {
        let dir = tempdir()?;
        let path = dir.path().join("edit-test.txt");
        tokio::fs::write(&path, "Hello, world!").await?;
        let tool = EditTool::new(dir.path());

        let result = tool
            .execute(
                "1",
                json_map(&json!({
                    "path": "edit-test.txt",
                    "edits": [{"oldText": "world", "newText": "testing"}]
                })),
                CancellationToken::new(),
                ToolUpdates::noop(),
            )
            .await?;
        assert_eq!(
            text_of(&result),
            "Successfully replaced 1 block(s) in edit-test.txt."
        );
        assert_eq!(tokio::fs::read_to_string(&path).await?, "Hello, testing!");
        assert!(result.details.get("diff").is_some());
        assert!(result.details.get("patch").is_some());

        tokio::fs::write(&path, "alpha\nbeta\ngamma\ndelta\n").await?;
        let result = tool
            .execute(
                "2",
                json_map(&json!({
                    "path": "edit-test.txt",
                    "edits": [
                        {"oldText": "alpha\n", "newText": "ALPHA\n"},
                        {"oldText": "gamma\n", "newText": "GAMMA\n"}
                    ]
                })),
                CancellationToken::new(),
                ToolUpdates::noop(),
            )
            .await?;
        assert_eq!(
            text_of(&result),
            "Successfully replaced 2 block(s) in edit-test.txt."
        );
        assert_eq!(
            tokio::fs::read_to_string(&path).await?,
            "ALPHA\nbeta\nGAMMA\ndelta\n"
        );
        Ok(())
    }

    #[tokio::test]
    async fn occurrence_and_overlap_errors() -> Result<(), Box<dyn std::error::Error>> {
        let dir = tempdir()?;
        let path = dir.path().join("dups.txt");
        tokio::fs::write(&path, "foo foo foo").await?;
        let tool = EditTool::new(dir.path());
        let err = tool
            .execute(
                "1",
                json_map(&json!({
                    "path": "dups.txt",
                    "edits": [{"oldText": "foo", "newText": "bar"}]
                })),
                CancellationToken::new(),
                ToolUpdates::noop(),
            )
            .await
            .err()
            .ok_or("duplicate edit was accepted")?;
        assert!(err.message().contains("Found 3 occurrences"));

        tokio::fs::write(&path, "one\ntwo\nthree\n").await?;
        let err = tool
            .execute(
                "2",
                json_map(&json!({
                    "path": "dups.txt",
                    "edits": [
                        {"oldText": "one\ntwo\n", "newText": "ONE\nTWO\n"},
                        {"oldText": "two\nthree\n", "newText": "TWO\nTHREE\n"}
                    ]
                })),
                CancellationToken::new(),
                ToolUpdates::noop(),
            )
            .await
            .err()
            .ok_or("overlapping edits were accepted")?;
        assert!(err.message().contains("overlap"));
        Ok(())
    }

    #[tokio::test]
    async fn reverse_apply_original_coordinates() -> Result<(), Box<dyn std::error::Error>> {
        let dir = tempdir()?;
        let path = dir.path().join("orig.txt");
        tokio::fs::write(&path, "foo\nbar\nbaz\n").await?;
        let tool = EditTool::new(dir.path());
        tool.execute(
            "1",
            json_map(&json!({
                "path": "orig.txt",
                "edits": [
                    {"oldText": "foo\n", "newText": "foo bar\n"},
                    {"oldText": "bar\n", "newText": "BAR\n"}
                ]
            })),
            CancellationToken::new(),
            ToolUpdates::noop(),
        )
        .await?;
        assert_eq!(
            tokio::fs::read_to_string(&path).await?,
            "foo bar\nBAR\nbaz\n"
        );
        Ok(())
    }

    #[tokio::test]
    async fn bom_and_crlf_preserved() -> Result<(), Box<dyn std::error::Error>> {
        let dir = tempdir()?;
        let path = dir.path().join("bom.txt");
        tokio::fs::write(&path, "\u{FEFF}first\r\nsecond\r\nthird\r\n").await?;
        let tool = EditTool::new(dir.path());
        tool.execute(
            "1",
            json_map(&json!({
                "path": "bom.txt",
                "edits": [{"oldText": "second\n", "newText": "REPLACED\n"}]
            })),
            CancellationToken::new(),
            ToolUpdates::noop(),
        )
        .await?;
        assert_eq!(
            tokio::fs::read_to_string(&path).await?,
            "\u{FEFF}first\r\nREPLACED\r\nthird\r\n"
        );
        Ok(())
    }

    #[tokio::test]
    async fn fuzzy_punctuation_and_space() -> Result<(), Box<dyn std::error::Error>> {
        let dir = tempdir()?;
        let path = dir.path().join("fuzzy.txt");
        tokio::fs::write(
            &path,
            "console.log(\u{2018}hello\u{2019});\nhello\u{00A0}world\n",
        )
        .await?;
        let tool = EditTool::new(dir.path());
        tool.execute(
            "1",
            json_map(&json!({
                "path": "fuzzy.txt",
                "edits": [
                    {"oldText": "console.log('hello');\n", "newText": "console.log('world');\n"},
                    {"oldText": "hello world\n", "newText": "hello universe\n"}
                ]
            })),
            CancellationToken::new(),
            ToolUpdates::noop(),
        )
        .await?;
        assert_eq!(
            tokio::fs::read_to_string(&path).await?,
            "console.log('world');\nhello universe\n"
        );
        Ok(())
    }

    #[tokio::test]
    async fn fuzzy_preserves_untouched_trailing_whitespace()
    -> Result<(), Box<dyn std::error::Error>> {
        let dir = tempdir()?;
        let path = dir.path().join("ws.txt");
        // untouched line keeps trailing spaces
        let original = ["keep before  ", "target line  ", "keep after  ", ""].join("\n");
        tokio::fs::write(&path, &original).await?;
        let tool = EditTool::new(dir.path());
        tool.execute(
            "1",
            json_map(&json!({
                "path": "ws.txt",
                "edits": [{"oldText": "target line\n", "newText": "changed\n"}]
            })),
            CancellationToken::new(),
            ToolUpdates::noop(),
        )
        .await?;
        let expected = ["keep before  ", "changed", "keep after  ", ""].join("\n");
        assert_eq!(tokio::fs::read_to_string(&path).await?, expected);
        Ok(())
    }

    #[tokio::test]
    async fn unchanged_no_op_rejected() -> Result<(), Box<dyn std::error::Error>> {
        let dir = tempdir()?;
        let path = dir.path().join("same.txt");
        tokio::fs::write(&path, "same").await?;
        let tool = EditTool::new(dir.path());
        let err = tool
            .execute(
                "1",
                json_map(&json!({
                    "path": "same.txt",
                    "edits": [{"oldText": "same", "newText": "same"}]
                })),
                CancellationToken::new(),
                ToolUpdates::noop(),
            )
            .await
            .err()
            .ok_or("unchanged edit was accepted")?;
        assert!(err.message().contains("No changes made"));
        Ok(())
    }

    #[tokio::test]
    async fn cancellation_before_work_aborts() -> Result<(), Box<dyn std::error::Error>> {
        let dir = tempdir()?;
        let tool = EditTool::new(dir.path());
        let cancel = CancellationToken::new();
        cancel.cancel();
        let err = tool
            .execute(
                "1",
                json_map(&json!({
                    "path": "a.txt",
                    "edits": [{"oldText": "a", "newText": "b"}]
                })),
                cancel,
                ToolUpdates::noop(),
            )
            .await
            .err()
            .ok_or("cancelled edit succeeded")?;
        assert_eq!(err.message(), "Operation aborted");
        Ok(())
    }

    #[tokio::test]
    async fn missing_file_enoent() -> Result<(), Box<dyn std::error::Error>> {
        let dir = tempdir()?;
        let tool = EditTool::new(dir.path());
        let err = tool
            .execute(
                "1",
                json_map(&json!({
                    "path": "missing.txt",
                    "edits": [{"oldText": "a", "newText": "b"}]
                })),
                CancellationToken::new(),
                ToolUpdates::noop(),
            )
            .await
            .err()
            .ok_or("missing file edit succeeded")?;
        assert!(err.message().contains("Error code: ENOENT"));
        Ok(())
    }

    #[tokio::test]
    async fn readonly_eacces() -> Result<(), Box<dyn std::error::Error>> {
        let dir = tempdir()?;
        let path = dir.path().join("ro.txt");
        tokio::fs::write(&path, "hello\n").await?;
        let mut perms = tokio::fs::metadata(&path).await?.permissions();
        perms.set_mode(0o444);
        tokio::fs::set_permissions(&path, perms).await?;
        let tool = EditTool::new(dir.path());
        let err = tool
            .execute(
                "1",
                json_map(&json!({
                    "path": "ro.txt",
                    "edits": [{"oldText": "hello", "newText": "world"}]
                })),
                CancellationToken::new(),
                ToolUpdates::noop(),
            )
            .await
            .err()
            .ok_or("readonly file edit succeeded")?;
        assert!(err.message().contains("Error code: EACCES"));
        // restore for tempdir cleanup
        let mut perms = tokio::fs::metadata(&path).await?.permissions();
        perms.set_mode(0o644);
        tokio::fs::set_permissions(&path, perms).await?;
        Ok(())
    }

    #[tokio::test]
    async fn mutation_queue_serializes_same_path() -> Result<(), Box<dyn std::error::Error>> {
        let dir = tempdir()?;
        let path = dir.path().join("serial.txt");
        tokio::fs::write(&path, "alpha\nbeta\n").await?;
        let tool = Arc::new(EditTool::new(dir.path()));
        let barrier = Arc::new(Barrier::new(2));

        let t1 = {
            let tool = tool.clone();
            let barrier = barrier.clone();
            tokio::spawn(async move {
                barrier.wait().await;
                tool.execute(
                    "1",
                    json_map(&json!({
                        "path": "serial.txt",
                        "edits": [{"oldText": "alpha", "newText": "ALPHA"}]
                    })),
                    CancellationToken::new(),
                    ToolUpdates::noop(),
                )
                .await
            })
        };
        let t2 = {
            let tool = tool.clone();
            let barrier = barrier.clone();
            tokio::spawn(async move {
                barrier.wait().await;
                // slight delay so both race into queue
                tokio::time::sleep(Duration::from_millis(5)).await;
                tool.execute(
                    "2",
                    json_map(&json!({
                        "path": "serial.txt",
                        "edits": [{"oldText": "beta", "newText": "BETA"}]
                    })),
                    CancellationToken::new(),
                    ToolUpdates::noop(),
                )
                .await
            })
        };
        t1.await??;
        t2.await??;
        assert_eq!(tokio::fs::read_to_string(&path).await?, "ALPHA\nBETA\n");
        Ok(())
    }
}
