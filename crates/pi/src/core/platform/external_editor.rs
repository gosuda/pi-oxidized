//! External editor lifecycle with cancellation and guaranteed temporary-file cleanup.

use std::future::Future;
use std::io;
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::process::Stdio;

use thiserror::Error;
use tokio::process::Command;
use tokio_util::sync::CancellationToken;
use uuid::Uuid;

use super::command::CommandSpec;

/// External editor completion.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum EditOutcome {
    /// Editor exited successfully but the content did not change.
    Unchanged,
    /// Editor exited successfully and returned replacement content.
    Changed(String),
    /// Caller cancelled the editor; the child was terminated and reaped.
    Aborted,
}

/// Editor launch or temporary-file failure.
#[derive(Debug, Error)]
pub enum EditorError {
    /// Empty command has no executable.
    #[error("external editor command is empty")]
    EmptyCommand,
    /// Temporary-file operation failed.
    #[error("external editor temporary file {path}: {source}")]
    TemporaryFile {
        /// Affected path.
        path: PathBuf,
        /// Underlying filesystem failure.
        #[source]
        source: io::Error,
    },
    /// Process operation failed.
    #[error("external editor process failed: {0}")]
    Process(#[from] io::Error),
}

/// Exit reported by an injected editor runner.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum EditorExit {
    /// Normal process exit with its code (`-1` when unavailable).
    Code(i32),
    /// Cancellation terminated the process.
    Aborted,
}

/// Injectable asynchronous editor process boundary.
pub trait EditorRunner {
    /// Run one editor command until exit or cancellation.
    fn run<'a>(
        &'a mut self,
        command: &'a CommandSpec,
        cancel: &'a CancellationToken,
    ) -> Pin<Box<dyn Future<Output = io::Result<EditorExit>> + Send + 'a>>;
}

/// Tokio-backed editor runner. Cancellation kills and reaps the child.
#[derive(Default)]
pub struct TokioEditorRunner;

impl EditorRunner for TokioEditorRunner {
    fn run<'a>(
        &'a mut self,
        command: &'a CommandSpec,
        cancel: &'a CancellationToken,
    ) -> Pin<Box<dyn Future<Output = io::Result<EditorExit>> + Send + 'a>> {
        Box::pin(async move {
            let mut child = Command::new(&command.program)
                .args(&command.args)
                .stdin(Stdio::inherit())
                .stdout(Stdio::inherit())
                .stderr(Stdio::inherit())
                .kill_on_drop(true)
                .spawn()?;
            tokio::select! {
                status = child.wait() => {
                    let status = status?;
                    Ok(EditorExit::Code(status.code().unwrap_or(-1)))
                }
                () = cancel.cancelled() => {
                    let _ = child.kill().await;
                    let _ = child.wait().await;
                    Ok(EditorExit::Aborted)
                }
            }
        })
    }
}

/// Convert a configured editor command and temporary path into typed argv.
///
/// This deliberately does not invoke a shell. Like the reference, whitespace
/// separates the executable and fixed arguments (`code --wait`).
///
/// # Errors
///
/// Returns [`EditorError::EmptyCommand`] for blank input.
pub fn external_editor_command(
    editor_command: &str,
    temporary_path: &Path,
) -> Result<CommandSpec, EditorError> {
    let mut words = editor_command.split_whitespace();
    let program = words.next().ok_or(EditorError::EmptyCommand)?;
    let mut args: Vec<String> = words.map(str::to_owned).collect();
    args.push(temporary_path.to_string_lossy().into_owned());
    Ok(CommandSpec::new(program, args))
}

/// Edit text using the host temporary directory and Tokio process runner.
///
/// # Errors
///
/// Returns temporary-file or process failures. The temporary file is removed
/// on success, nonzero exit, cancellation, and every error path.
pub async fn edit_text_in_external_editor(
    editor_command: &str,
    initial: &str,
    cancel: &CancellationToken,
) -> Result<EditOutcome, EditorError> {
    let mut runner = TokioEditorRunner;
    edit_text_in_external_editor_with(
        editor_command,
        initial,
        cancel,
        &std::env::temp_dir(),
        &Uuid::new_v4().to_string(),
        &mut runner,
    )
    .await
}

/// Injectable external-editor implementation.
///
/// # Errors
///
/// Returns [`EditorError::TemporaryFile`] when the temporary directory or file
/// cannot be created, written, or read, [`EditorError::EmptyCommand`] for a
/// blank editor command, and [`EditorError::Process`] when the editor process
/// cannot be run.
pub async fn edit_text_in_external_editor_with(
    editor_command: &str,
    initial: &str,
    cancel: &CancellationToken,
    temp_dir: &Path,
    unique_id: &str,
    runner: &mut dyn EditorRunner,
) -> Result<EditOutcome, EditorError> {
    let temp_path = temp_dir.join(format!("pi-editor-{unique_id}.pi.md"));
    std::fs::create_dir_all(temp_dir).map_err(|source| EditorError::TemporaryFile {
        path: temp_dir.to_path_buf(),
        source,
    })?;
    std::fs::write(&temp_path, initial).map_err(|source| EditorError::TemporaryFile {
        path: temp_path.clone(),
        source,
    })?;
    let cleanup = TemporaryFileGuard(temp_path.clone());
    let command = external_editor_command(editor_command, &temp_path)?;
    let exit = runner.run(&command, cancel).await?;
    if exit == EditorExit::Aborted {
        return Ok(EditOutcome::Aborted);
    }
    if exit != EditorExit::Code(0) {
        return Ok(EditOutcome::Unchanged);
    }
    let edited =
        std::fs::read_to_string(&temp_path).map_err(|source| EditorError::TemporaryFile {
            path: temp_path,
            source,
        })?;
    let edited = edited.strip_suffix('\n').unwrap_or(&edited).to_owned();
    let outcome = if edited == initial {
        EditOutcome::Unchanged
    } else {
        EditOutcome::Changed(edited)
    };
    drop(cleanup);
    Ok(outcome)
}

struct TemporaryFileGuard(PathBuf);

impl Drop for TemporaryFileGuard {
    fn drop(&mut self) {
        drop(std::fs::remove_file(&self.0));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    type TestResult = Result<(), Box<dyn std::error::Error>>;

    struct FakeRunner {
        exit: EditorExit,
        replacement: Option<String>,
        seen: Vec<CommandSpec>,
    }

    impl EditorRunner for FakeRunner {
        fn run<'a>(
            &'a mut self,
            command: &'a CommandSpec,
            _cancel: &'a CancellationToken,
        ) -> Pin<Box<dyn Future<Output = io::Result<EditorExit>> + Send + 'a>> {
            self.seen.push(command.clone());
            let replacement = self.replacement.clone();
            let path = command.args.last().map(PathBuf::from);
            let exit = self.exit;
            Box::pin(async move {
                if let (Some(text), Some(path)) = (replacement, path) {
                    std::fs::write(path, text)?;
                }
                Ok(exit)
            })
        }
    }

    #[test]
    fn argv_is_cross_platform_and_shell_free() -> TestResult {
        let path = Path::new("/tmp/message with spaces.md");
        let command = external_editor_command("code --wait", path)?;
        assert_eq!(command.program, "code");
        assert_eq!(command.args, vec!["--wait", "/tmp/message with spaces.md"]);
        Ok(())
    }

    #[tokio::test]
    async fn changed_unchanged_abort_and_cleanup() -> TestResult {
        let dir = tempfile::tempdir()?;
        let cancel = CancellationToken::new();
        let mut changed = FakeRunner {
            exit: EditorExit::Code(0),
            replacement: Some("after\n".to_owned()),
            seen: Vec::new(),
        };
        let result = edit_text_in_external_editor_with(
            "editor",
            "before",
            &cancel,
            dir.path(),
            "changed",
            &mut changed,
        )
        .await?;
        assert_eq!(result, EditOutcome::Changed("after".to_owned()));
        assert!(!dir.path().join("pi-editor-changed.pi.md").exists());

        let mut unchanged = FakeRunner {
            exit: EditorExit::Code(3),
            replacement: Some("ignored".to_owned()),
            seen: Vec::new(),
        };
        let result = edit_text_in_external_editor_with(
            "editor",
            "before",
            &cancel,
            dir.path(),
            "unchanged",
            &mut unchanged,
        )
        .await?;
        assert_eq!(result, EditOutcome::Unchanged);
        assert!(!dir.path().join("pi-editor-unchanged.pi.md").exists());

        let mut aborted = FakeRunner {
            exit: EditorExit::Aborted,
            replacement: None,
            seen: Vec::new(),
        };
        let result = edit_text_in_external_editor_with(
            "editor",
            "before",
            &cancel,
            dir.path(),
            "aborted",
            &mut aborted,
        )
        .await?;
        assert_eq!(result, EditOutcome::Aborted);
        assert!(!dir.path().join("pi-editor-aborted.pi.md").exists());
        Ok(())
    }
}
