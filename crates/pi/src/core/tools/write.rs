//! Write tool: create or overwrite a file under the mutation queue.
//!
//! Ports `.references/pi/packages/coding-agent/src/core/tools/write.ts`.
//! Success text reports a JS-style UTF-16 code-unit length as "bytes".

use std::path::{Path, PathBuf};
use std::sync::Arc;

use futures::FutureExt as _;
use futures::future::BoxFuture;
use pi_agent::{AgentTool, AgentToolResult, ToolError, ToolUpdates};
use pi_ai::types::{TextContent, ToolResultContent};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};
use tokio_util::sync::CancellationToken;

use super::{MutationQueueError, PathResolveError, resolve_to_cwd, with_file_mutation_queue};

/// TypeBox-compatible write arguments (fixture `write.json`).
#[derive(Clone, Debug, Deserialize, JsonSchema, PartialEq, Serialize)]
pub struct WriteToolInput {
    /// Path to the file to write (relative or absolute).
    #[schemars(description = "Path to the file to write (relative or absolute)")]
    pub path: String,
    /// Content to write to the file.
    #[schemars(description = "Content to write to the file")]
    pub content: String,
}

/// Write tool has no structured `details` payload.
pub type WriteToolDetails = ();

/// Options for [`WriteTool`].
#[derive(Clone, Debug)]
pub struct WriteToolOptions {
    /// Working directory used to resolve relative paths.
    pub cwd: PathBuf,
}

impl WriteToolOptions {
    /// Builds options for `cwd`.
    #[must_use]
    pub fn new(cwd: impl Into<PathBuf>) -> Self {
        Self { cwd: cwd.into() }
    }
}

/// Agent tool that writes UTF-8 file contents under the mutation queue.
#[derive(Clone, Debug)]
pub struct WriteTool {
    cwd: PathBuf,
    parameters: Value,
    description: String,
}

impl WriteTool {
    /// Creates a write tool rooted at `cwd`.
    #[must_use]
    pub fn new(cwd: impl Into<PathBuf>) -> Self {
        Self::with_options(WriteToolOptions::new(cwd))
    }

    /// Creates a write tool from explicit options.
    #[must_use]
    pub fn with_options(options: WriteToolOptions) -> Self {
        Self {
            cwd: options.cwd,
            parameters: write_parameters_schema(),
            description: "Write content to a file. Creates the file if it doesn't exist, overwrites if it does. Automatically creates parent directories.".to_owned(),
        }
    }

    /// Returns the JSON Schema for write arguments (normalized `TypeBox` shape).
    #[must_use]
    pub fn parameters_schema() -> Value {
        write_parameters_schema()
    }

    /// Validates raw tool arguments into [`WriteToolInput`].
    ///
    /// # Errors
    ///
    /// Returns [`ToolError`] when required fields are missing or mistyped.
    pub fn parse_input(args: &Map<String, Value>) -> Result<WriteToolInput, ToolError> {
        serde_json::from_value(Value::Object(args.clone()))
            .map_err(|error| ToolError::new(format!("Write tool input is invalid. {error}")))
    }

    /// JS `String.prototype.length` (UTF-16 code units).
    #[must_use]
    pub fn utf16_length(content: &str) -> usize {
        content.encode_utf16().count()
    }

    /// Formats the success text using the caller's path string and JS length.
    #[must_use]
    pub fn success_text(path: &str, content: &str) -> String {
        format!(
            "Successfully wrote {} bytes to {path}",
            Self::utf16_length(content)
        )
    }
}

impl AgentTool for WriteTool {
    fn name(&self) -> &'static str {
        "write"
    }

    fn label(&self) -> &'static str {
        "write"
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
            let input = WriteTool::parse_input(&args)?;
            let absolute_path = resolve_to_cwd(&input.path, cwd.to_string_lossy().as_ref())
                .map_err(|error| path_error(&error))?;
            let absolute = PathBuf::from(&absolute_path);
            let parent = absolute
                .parent()
                .map_or_else(|| PathBuf::from("."), Path::to_path_buf);
            let path_for_message = input.path.clone();
            let content = input.content;
            let cancel_for_queue = cancel.clone();

            with_file_mutation_queue(&absolute, || {
                let parent = parent.clone();
                let absolute = absolute.clone();
                let content = content.clone();
                let cancel = cancel_for_queue.clone();
                async move {
                    apply_write_mutation_with_commit_hooks(
                        &parent,
                        &absolute,
                        &path_for_message,
                        &content,
                        &cancel,
                        || {},
                        || {},
                    )
                    .await
                }
            })
            .await
            .map_err(|error| mutation_error(&error))?
        }
        .boxed()
    }
}

async fn apply_write_mutation_with_commit_hooks<BeforeCommit, AfterCommit>(
    parent: &Path,
    absolute: &Path,
    path_for_message: &str,
    content: &str,
    cancel: &CancellationToken,
    before_commit: BeforeCommit,
    after_commit: AfterCommit,
) -> Result<AgentToolResult, ToolError>
where
    BeforeCommit: FnOnce() + Send,
    AfterCommit: FnOnce() + Send,
{
    throw_if_cancelled(cancel)?;

    tokio::fs::create_dir_all(parent).await.map_err(|error| {
        ToolError::new(format!(
            "Could not create parent directories for {}: {error}",
            absolute.display()
        ))
    })?;

    let result = AgentToolResult {
        content: vec![ToolResultContent::Text(TextContent::new(
            WriteTool::success_text(path_for_message, content),
        ))],
        details: Value::Null,
        added_tool_names: None,
        terminate: None,
    };

    before_commit();
    throw_if_cancelled(cancel)?;
    tokio::fs::write(absolute, content.as_bytes())
        .await
        .map_err(|error| {
            ToolError::new(format!(
                "Could not write file {}: {error}",
                absolute.display()
            ))
        })?;
    after_commit();

    // A successful write is the durable commit point. Cancellation observed
    // after it must not turn the committed write into a reported failure.
    Ok(result)
}

fn write_parameters_schema() -> Value {
    normalize_tool_schema(schemars::schema_for!(WriteToolInput))
}

fn normalize_tool_schema(schema: schemars::Schema) -> Value {
    let mut value = serde_json::to_value(schema).unwrap_or_else(|_| Value::Object(Map::new()));
    if let Value::Object(map) = &mut value {
        map.remove("$schema");
        map.remove("title");
        map.remove("description");
        map.remove("additionalProperties");
        normalize_schema_node(map);
    }
    value
}

fn normalize_schema_node(map: &mut Map<String, Value>) {
    map.remove("format");
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

fn mutation_error(error: &MutationQueueError) -> ToolError {
    ToolError::new(error.to_string())
}

/// Builds an [`Arc<dyn AgentTool>`] write tool for `cwd`.
#[must_use]
pub fn create_write_tool(cwd: impl Into<PathBuf>) -> Arc<dyn AgentTool> {
    Arc::new(WriteTool::new(cwd))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;
    use std::time::Duration;

    use serde_json::json;
    use tempfile::tempdir;
    use tokio::sync::Barrier;

    fn fixture_schema() -> Result<Value, serde_json::Error> {
        let text = include_str!("../../../tests/fixtures/tool-schemas/write.json");
        serde_json::from_str(text)
    }

    #[test]
    fn schema_matches_typebox_fixture() -> Result<(), Box<dyn std::error::Error>> {
        let schema = WriteTool::parameters_schema();
        assert_eq!(schema, fixture_schema()?);
        Ok(())
    }

    #[test]
    fn utf16_length_counts_surrogate_pairs() {
        // "😀" is one scalar, two UTF-16 code units; JS length is 2.
        assert_eq!(WriteTool::utf16_length("😀"), 2);
        assert_eq!(WriteTool::utf16_length("abc"), 3);
        assert_eq!(
            WriteTool::success_text("a.txt", "😀"),
            "Successfully wrote 2 bytes to a.txt"
        );
    }

    #[tokio::test]
    async fn writes_create_parent_and_overwrite() -> Result<(), Box<dyn std::error::Error>> {
        let dir = tempdir()?;
        let tool = WriteTool::new(dir.path());
        let nested = "sub/dir/file.txt";
        let args = json_map(json!({
            "path": nested,
            "content": "hello"
        }))?;
        let result = tool
            .execute("1", args, CancellationToken::new(), ToolUpdates::noop())
            .await?;
        let text = text_of(&result);
        assert_eq!(text, "Successfully wrote 5 bytes to sub/dir/file.txt");
        let path = dir.path().join(nested);
        assert_eq!(tokio::fs::read_to_string(&path).await?, "hello");

        let args = json_map(json!({
            "path": nested,
            "content": "overwrite"
        }))?;
        let result = tool
            .execute("2", args, CancellationToken::new(), ToolUpdates::noop())
            .await?;
        assert_eq!(
            text_of(&result),
            "Successfully wrote 9 bytes to sub/dir/file.txt"
        );
        assert_eq!(tokio::fs::read_to_string(&path).await?, "overwrite");
        Ok(())
    }

    #[tokio::test]
    async fn cancellation_before_work_aborts() -> Result<(), Box<dyn std::error::Error>> {
        let dir = tempdir()?;
        let tool = WriteTool::new(dir.path());
        let cancel = CancellationToken::new();
        cancel.cancel();
        let result = tool
            .execute(
                "1",
                json_map(json!({"path": "a.txt", "content": "x"}))?,
                cancel,
                ToolUpdates::noop(),
            )
            .await;
        let Err(err) = result else {
            return Err("expected cancellation error".into());
        };
        assert_eq!(err.message(), "Operation aborted");
        Ok(())
    }

    #[tokio::test]
    async fn cancellation_before_write_commit_aborts_without_mutating()
    -> Result<(), Box<dyn std::error::Error>> {
        let dir = tempdir()?;
        let path = dir.path().join("pre-commit.txt");
        tokio::fs::write(&path, "before").await?;
        let cancel = CancellationToken::new();
        let cancel_at_boundary = cancel.clone();

        let result = apply_write_mutation_with_commit_hooks(
            dir.path(),
            &path,
            "pre-commit.txt",
            "after",
            &cancel,
            move || cancel_at_boundary.cancel(),
            || {},
        )
        .await;
        let Err(error) = result else {
            return Err("pre-commit cancellation unexpectedly succeeded".into());
        };

        assert_eq!(error.message(), "Operation aborted");
        assert_eq!(tokio::fs::read_to_string(&path).await?, "before");
        Ok(())
    }

    #[tokio::test]
    async fn cancellation_after_write_commit_reports_success()
    -> Result<(), Box<dyn std::error::Error>> {
        let dir = tempdir()?;
        let path = dir.path().join("post-commit.txt");
        let cancel = CancellationToken::new();
        let cancel_at_boundary = cancel.clone();

        let result = apply_write_mutation_with_commit_hooks(
            dir.path(),
            &path,
            "post-commit.txt",
            "committed",
            &cancel,
            || {},
            move || cancel_at_boundary.cancel(),
        )
        .await?;

        assert!(cancel.is_cancelled());
        assert_eq!(
            text_of(&result),
            "Successfully wrote 9 bytes to post-commit.txt"
        );
        assert_eq!(tokio::fs::read_to_string(&path).await?, "committed");
        Ok(())
    }

    #[tokio::test]
    async fn serialized_writes_same_path() -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        let dir = tempdir()?;
        let path = dir.path().join("serial.txt");
        let barrier = Arc::new(Barrier::new(2));
        let order = Arc::new(std::sync::Mutex::new(Vec::new()));

        let start_a = barrier.clone();
        let order_a = order.clone();
        let path_a = path.clone();
        let a = tokio::spawn(async move {
            with_file_mutation_queue(&path_a, || {
                let start_a = start_a.clone();
                let order_a = order_a.clone();
                let path_a = path_a.clone();
                async move {
                    start_a.wait().await;
                    order_a
                        .lock()
                        .map_err(|_| std::io::Error::other("lock poisoned"))?
                        .push("a-start");
                    tokio::time::sleep(Duration::from_millis(40)).await;
                    tokio::fs::write(&path_a, b"a").await?;
                    order_a
                        .lock()
                        .map_err(|_| std::io::Error::other("lock poisoned"))?
                        .push("a-end");
                    Ok::<(), std::io::Error>(())
                }
            })
            .await
            .map_err(|error| std::io::Error::other(error.to_string()))?
        });

        let start_b = barrier;
        let order_b = order.clone();
        let path_b = path.clone();
        let b = tokio::spawn(async move {
            // Ensure both tasks race to acquire.
            start_b.wait().await;
            with_file_mutation_queue(&path_b, || {
                let order_b = order_b.clone();
                let path_b = path_b.clone();
                async move {
                    order_b
                        .lock()
                        .map_err(|_| std::io::Error::other("lock poisoned"))?
                        .push("b-start");
                    tokio::fs::write(&path_b, b"b").await?;
                    order_b
                        .lock()
                        .map_err(|_| std::io::Error::other("lock poisoned"))?
                        .push("b-end");
                    Ok::<(), std::io::Error>(())
                }
            })
            .await
            .map_err(|error| std::io::Error::other(error.to_string()))?
        });

        a.await??;
        b.await??;
        let observed = order
            .lock()
            .map_err(|_| std::io::Error::other("lock poisoned"))?
            .clone();
        // FIFO: a fully finishes before b starts, or b fully finishes before a
        // starts — never interleaved start/end across the critical section.
        assert!(
            observed == ["a-start", "a-end", "b-start", "b-end"]
                || observed == ["b-start", "b-end", "a-start", "a-end"],
            "unexpected order: {observed:?}"
        );
        Ok(())
    }

    fn json_map(value: Value) -> Result<Map<String, Value>, &'static str> {
        if let Value::Object(map) = value {
            Ok(map)
        } else {
            Err("expected JSON object")
        }
    }

    fn text_of(result: &AgentToolResult) -> String {
        match result.content.first() {
            Some(ToolResultContent::Text(text)) => text.text.clone(),
            _ => String::new(),
        }
    }
}
