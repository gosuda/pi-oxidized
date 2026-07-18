//! Non-interactive prompt assembly: stdin/argv merge and `@file` expansion.
//!
//! Ports the input half of `.references/pi/packages/coding-agent/src/modes/
//! print-mode.ts` together with `cli/file-processor.ts`, `cli/initial-message.ts`,
//! and the piped-stdin reader in `main.ts`. File argument expansion reuses the
//! shared image pipeline in [`crate::core::tools::read`]
//! (`detect_supported_image_mime_type` and `process_image_bytes`) and the
//! macOS-aware path resolver in [`crate::core::tools::path_utils`], so print
//! input and the `read` tool stay byte-for-byte consistent.
//!
//! `@@` is a literal escape: an argument the CLI parser already stripped one
//! leading `@` from (`@@foo` -> file arg `@foo`) is treated as literal prompt
//! text rather than a file reference, so users can pass prompts that start with
//! `@`.

use std::io;
use std::path::PathBuf;

use pi_ai::ImageContent;
use thiserror::Error;
use tokio::fs;
use tokio::io::{AsyncRead, AsyncReadExt};

use crate::core::tools::path_utils::{PathResolveError, resolve_read_path_async};
use crate::core::tools::read::{detect_supported_image_mime_type, process_image_bytes};

/// Errors produced while assembling the non-interactive prompt.
#[derive(Debug, Error)]
pub enum PrintInputError {
    /// A referenced `@file` does not exist on disk.
    #[error("File not found: {0}")]
    FileNotFound(PathBuf),
    /// A text `@file` exists but could not be read.
    #[error("Could not read file {path}: {message}")]
    FileNotReadable {
        /// Absolute path of the unreadable file.
        path: PathBuf,
        /// Underlying read failure description.
        message: String,
    },
    /// Path expansion failed (bad `file://` URL, no home directory, …).
    #[error(transparent)]
    PathResolve(#[from] PathResolveError),
    /// Raw I/O failure not covered by the typed variants above.
    #[error(transparent)]
    Io(#[from] io::Error),
}

/// Text and image attachments produced by expanding `@file` arguments.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct ProcessedFiles {
    /// Concatenated `<file>` blocks and `@@` literal fragments.
    pub text: String,
    /// Inline image attachments ready for a user message.
    pub images: Vec<ImageContent>,
}

/// Options for [`process_file_arguments`].
#[derive(Clone, Copy, Debug)]
pub struct ProcessFileOptions {
    /// Whether to auto-resize images to the 2000×2000 / 4.5 MiB inline limits.
    pub auto_resize_images: bool,
}

impl Default for ProcessFileOptions {
    fn default() -> Self {
        Self {
            auto_resize_images: true,
        }
    }
}

/// The assembled initial prompt plus deferred follow-up messages.
///
/// Mirrors the TypeScript `buildInitialMessage` result: the first CLI message
/// is consumed into [`initial_message`](Self::initial_message); the rest stay in
/// [`remaining_messages`](Self::remaining_messages) for later prompts.
#[derive(Clone, Debug, Default)]
pub struct PromptSource {
    /// Joined prompt for the first `session.prompt` call (`None` when empty).
    pub initial_message: Option<String>,
    /// Images attached to the initial prompt.
    pub initial_images: Vec<ImageContent>,
    /// Remaining CLI messages to send after the initial prompt.
    pub remaining_messages: Vec<String>,
}

/// Combine piped stdin, `@file` text, and the first CLI message.
///
/// Join order matches `buildInitialMessage`: `stdin_content` (even an empty
/// string when `Some`) first, then `file_text` (only when non-empty), then the
/// first entry of `messages` (which is removed in place for later prompts).
///
/// # Errors
///
/// This function is infallible; the `Result` is reserved for future expansion
/// and always returns [`Ok`].
pub fn build_initial_message(
    messages: &mut Vec<String>,
    stdin_content: Option<&str>,
    file_text: Option<&str>,
    file_images: Vec<ImageContent>,
) -> PromptSource {
    let mut parts: Vec<String> = Vec::new();
    if let Some(stdin) = stdin_content {
        parts.push(stdin.to_owned());
    }
    if let Some(text) = file_text
        && !text.is_empty()
    {
        parts.push(text.to_owned());
    }
    if !messages.is_empty() {
        parts.push(messages.remove(0));
    }

    let initial_message = if parts.is_empty() {
        None
    } else {
        Some(parts.concat())
    };
    let initial_images = if file_images.is_empty() {
        Vec::new()
    } else {
        file_images
    };

    PromptSource {
        initial_message,
        initial_images,
        remaining_messages: Vec::new(),
    }
}

/// Expand `@file` arguments into text content and image attachments.
///
/// Each argument is resolved against `cwd` (with `~`, `file://`, Unicode-space,
/// and macOS screenshot variants applied by the shared resolver). Missing files
/// fail with [`PrintInputError::FileNotFound`]; empty files are skipped.
/// Supported image bytes run through the shared resize pipeline and produce an
/// [`ImageContent`] plus a `<file>` hint block; failed conversions emit the
/// exact omission notice without an attachment. Text files are wrapped in
/// `<file name="…">\n…\n</file>\n`.
///
/// A leading `@` in an argument (i.e. the user wrote `@@`) is a literal escape:
/// the argument is appended verbatim to [`ProcessedFiles::text`] instead of
/// being treated as a path.
///
/// # Errors
///
/// See [`PrintInputError`].
pub async fn process_file_arguments(
    file_args: &[String],
    cwd: &str,
    options: ProcessFileOptions,
) -> Result<ProcessedFiles, PrintInputError> {
    let mut text = String::new();
    let mut images: Vec<ImageContent> = Vec::new();

    for file_arg in file_args {
        // `@@` literal escape: the CLI parser already stripped one `@`, so a
        // leading `@` here means the user wrote `@@<text>` for a literal.
        if let Some(literal) = file_arg.strip_prefix('@') {
            text.push('@');
            text.push_str(literal);
            continue;
        }

        let resolved = resolve_read_path_async(file_arg, cwd).await?;
        let absolute_path = PathBuf::from(&resolved);

        if !fs::try_exists(&absolute_path).await.unwrap_or(false) {
            return Err(PrintInputError::FileNotFound(absolute_path));
        }

        let metadata = match fs::metadata(&absolute_path).await {
            Ok(meta) => meta,
            Err(err) if err.kind() == io::ErrorKind::NotFound => {
                return Err(PrintInputError::FileNotFound(absolute_path));
            }
            Err(err) => {
                return Err(PrintInputError::FileNotReadable {
                    path: absolute_path,
                    message: err.to_string(),
                });
            }
        };
        if metadata.len() == 0 {
            // Skip empty files, matching the TypeScript `stat().size === 0` branch.
            continue;
        }

        let bytes = match fs::read(&absolute_path).await {
            Ok(bytes) => bytes,
            Err(err) => {
                return Err(PrintInputError::FileNotReadable {
                    path: absolute_path,
                    message: err.to_string(),
                });
            }
        };

        let Some(mime_type) = detect_supported_image_mime_type(&bytes) else {
            // Text file: wrap content verbatim. Node `readFile` UTF-8 replaces
            // invalid sequences with U+FFFD; `from_utf8_lossy` matches that.
            let content = String::from_utf8_lossy(&bytes);
            text.push_str("<file name=\"");
            text.push_str(&resolved);
            text.push_str("\">\n");
            text.push_str(&content);
            text.push_str("\n</file>\n");
            continue;
        };

        match process_image_bytes(&bytes, &mime_type, options.auto_resize_images) {
            crate::core::tools::read::ProcessImageResult::Ok(processed) => {
                images.push(ImageContent::new(processed.data, processed.mime_type));
                text.push_str("<file name=\"");
                text.push_str(&resolved);
                text.push_str("\">");
                if !processed.hints.is_empty() {
                    text.push_str(&processed.hints.join("\n"));
                }
                text.push_str("</file>\n");
            }
            crate::core::tools::read::ProcessImageResult::Failed(failed) => {
                text.push_str("<file name=\"");
                text.push_str(&resolved);
                text.push_str("\">");
                text.push_str(&failed.message);
                text.push_str("</file>\n");
            }
        }
    }

    Ok(ProcessedFiles { text, images })
}

/// Read piped stdin into a trimmed string.
///
/// When `is_tty` is true the caller is an interactive terminal and nothing is
/// read (`None`). Otherwise the full stream is read, trimmed of surrounding
/// whitespace, and returned as `Some` (including an empty string when the input
/// was whitespace-only). The trimmed-empty case maps to `None`, matching the
/// TypeScript `data.trim() || undefined` reader in `main.ts`.
///
/// # Errors
///
/// Returns I/O failures from the underlying read.
pub async fn read_piped_stdin<R>(is_tty: bool, reader: R) -> io::Result<Option<String>>
where
    R: AsyncRead + Unpin,
{
    if is_tty {
        return Ok(None);
    }
    let mut buf = Vec::new();
    let mut reader = reader;
    reader.read_to_end(&mut buf).await?;
    let lossy = String::from_utf8_lossy(&buf).into_owned();
    let trimmed = lossy.trim();
    if trimmed.is_empty() {
        Ok(None)
    } else {
        Ok(Some(trimmed.to_owned()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    type TestResult = Result<(), Box<dyn std::error::Error>>;

    #[test]
    fn build_initial_message_empty_returns_none() {
        let mut messages = Vec::new();
        let src = build_initial_message(&mut messages, None, None, Vec::new());
        assert!(src.initial_message.is_none());
        assert!(src.initial_images.is_empty());
        assert!(src.remaining_messages.is_empty());
    }

    #[test]
    fn build_initial_message_joins_stdin_file_text_first_message() {
        let mut messages = vec!["third".to_owned()];
        let src = build_initial_message(
            &mut messages,
            Some("stdin"),
            Some("<file>body</file>\n"),
            Vec::new(),
        );
        assert_eq!(
            src.initial_message.as_deref(),
            Some("stdin<file>body</file>\nthird")
        );
        assert!(messages.is_empty(), "first message consumed");
    }

    #[test]
    fn build_initial_message_empty_stdin_still_joined() {
        let mut messages = vec!["msg".to_owned()];
        let src = build_initial_message(&mut messages, Some(""), None, Vec::new());
        assert_eq!(src.initial_message.as_deref(), Some("msg"));
    }

    #[test]
    fn build_initial_message_preserves_remaining_messages() {
        let mut messages = vec!["first".to_owned(), "second".to_owned()];
        let src = build_initial_message(&mut messages, None, None, Vec::new());
        assert_eq!(src.initial_message.as_deref(), Some("first"));
        assert_eq!(messages, vec!["second".to_owned()]);
    }

    #[test]
    fn build_initial_message_carries_images() {
        let img = ImageContent::new("AA==", "image/png");
        let mut messages = Vec::new();
        let src = build_initial_message(&mut messages, None, None, vec![img.clone()]);
        assert_eq!(src.initial_images, vec![img]);
    }

    #[tokio::test]
    async fn process_file_arguments_missing_file_typed_error() {
        let result = process_file_arguments(
            &["definitely/missing/file.xyz".to_owned()],
            "/tmp",
            ProcessFileOptions::default(),
        )
        .await;
        assert!(
            matches!(result, Err(PrintInputError::FileNotFound(_))),
            "{result:?}"
        );
    }

    #[tokio::test]
    async fn process_file_arguments_text_file_wrapped() -> TestResult {
        let dir = tempfile::tempdir()?;
        let path = dir.path().join("note.txt");
        fs::write(&path, "hello world").await?;
        let arg = path.to_string_lossy().to_string();
        let processed = process_file_arguments(&[arg], "/", ProcessFileOptions::default()).await?;
        assert!(processed.images.is_empty());
        assert_eq!(
            processed.text,
            format!("<file name=\"{}\">\nhello world\n</file>\n", path.display())
        );
        Ok(())
    }

    #[tokio::test]
    async fn process_file_arguments_skips_empty_file() -> TestResult {
        let dir = tempfile::tempdir()?;
        let path = dir.path().join("empty.txt");
        fs::write(&path, "").await?;
        let arg = path.to_string_lossy().to_string();
        let processed = process_file_arguments(&[arg], "/", ProcessFileOptions::default()).await?;
        assert!(processed.text.is_empty());
        assert!(processed.images.is_empty());
        Ok(())
    }

    #[tokio::test]
    async fn process_file_arguments_at_at_is_literal() -> TestResult {
        // `@@literal` arrives as `@literal` after the CLI parser strips one `@`.
        let processed = process_file_arguments(
            &["@literal".to_owned()],
            "/tmp",
            ProcessFileOptions::default(),
        )
        .await?;
        assert_eq!(processed.text, "@literal");
        assert!(processed.images.is_empty());
        Ok(())
    }

    #[tokio::test]
    async fn process_file_arguments_unreadable_file_typed_error() -> TestResult {
        // A directory is not readable as a file via `fs::read`.
        let dir = tempfile::tempdir()?;
        let arg = dir.path().to_string_lossy().to_string();
        let result = process_file_arguments(&[arg], "/", ProcessFileOptions::default()).await;
        assert!(
            matches!(
                result,
                Err(PrintInputError::Io(_) | PrintInputError::FileNotReadable { .. })
            ),
            "{result:?}"
        );
        Ok(())
    }

    #[tokio::test]
    async fn read_piped_stdin_tty_returns_none() -> TestResult {
        let result = read_piped_stdin(true, Cursor::new(b"ignored")).await?;
        assert_eq!(result, None);
        Ok(())
    }

    #[tokio::test]
    async fn read_piped_stdin_reads_and_trims() -> TestResult {
        let result = read_piped_stdin(false, Cursor::new(b"  hello\n  ")).await?;
        assert_eq!(result.as_deref(), Some("hello"));
        Ok(())
    }

    #[tokio::test]
    async fn read_piped_stdin_empty_returns_none() -> TestResult {
        let result = read_piped_stdin(false, Cursor::new(b"   \n")).await?;
        assert_eq!(result, None);
        Ok(())
    }
}
