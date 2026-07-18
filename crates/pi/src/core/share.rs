//! Secret-gist sharing through an injected `gh` command runner.

use std::io;
use std::path::{Path, PathBuf};
use std::process::Stdio;

use futures::future::BoxFuture;
#[cfg(test)]
use std::collections::VecDeque;
#[cfg(test)]
use std::sync::Mutex;
use thiserror::Error;
use tokio::process::Command;
use tokio_util::sync::CancellationToken;
use uuid::Uuid;

use super::config::get_share_viewer_url;
use super::export_html::{ExportError, ExportOptions, SessionExportState, export_session_to_html};
use super::sessions::SessionManager;

/// Captured command completion.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct CommandOutput {
    /// Process exit code (`None` when unavailable).
    pub status: Option<i32>,
    /// Captured standard output.
    pub stdout: String,
    /// Captured standard error.
    pub stderr: String,
}

impl CommandOutput {
    fn success(&self) -> bool {
        self.status == Some(0)
    }
}

/// Command-launch failures classified independently from process exit status.
#[derive(Debug, Error)]
pub enum CommandRunError {
    /// Executable was not found.
    #[error("command not found: {0}")]
    NotFound(String),
    /// Command I/O failed.
    #[error(transparent)]
    Io(#[from] io::Error),
    /// The cancellation token fired.
    #[error("command cancelled")]
    Cancelled,
}

/// Injectable process boundary used by gist sharing.
pub trait CommandRunner: Send + Sync {
    /// Run one command with captured stdout/stderr and linked cancellation.
    fn run<'a>(
        &'a self,
        program: &'a str,
        arguments: &'a [String],
        cancellation: &'a CancellationToken,
    ) -> BoxFuture<'a, Result<CommandOutput, CommandRunError>>;
}

/// Production command runner backed by `tokio::process`.
#[derive(Clone, Copy, Debug, Default)]
pub struct SystemCommandRunner;

impl CommandRunner for SystemCommandRunner {
    fn run<'a>(
        &'a self,
        program: &'a str,
        arguments: &'a [String],
        cancellation: &'a CancellationToken,
    ) -> BoxFuture<'a, Result<CommandOutput, CommandRunError>> {
        Box::pin(async move {
            if cancellation.is_cancelled() {
                return Err(CommandRunError::Cancelled);
            }
            let mut command = Command::new(program);
            command
                .args(arguments)
                .stdin(Stdio::null())
                .stdout(Stdio::piped())
                .stderr(Stdio::piped())
                .kill_on_drop(true);
            let child = command.spawn().map_err(|error| {
                if error.kind() == io::ErrorKind::NotFound {
                    CommandRunError::NotFound(program.to_owned())
                } else {
                    CommandRunError::Io(error)
                }
            })?;
            let wait = child.wait_with_output();
            tokio::pin!(wait);
            let output = tokio::select! {
                biased;
                () = cancellation.cancelled() => return Err(CommandRunError::Cancelled),
                result = &mut wait => result.map_err(CommandRunError::Io)?,
            };
            Ok(CommandOutput {
                status: output.status.code(),
                stdout: String::from_utf8_lossy(&output.stdout).into_owned(),
                stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
            })
        })
    }
}

/// Successful secret-gist share URLs.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ShareResult {
    /// `{PI_SHARE_VIEWER_URL or default}#{gistId}`.
    pub viewer_url: String,
    /// Raw gist URL selected from the final output line.
    pub gist_url: String,
}

impl ShareResult {
    /// Exact two-line interactive status text.
    #[must_use]
    pub fn status_text(&self) -> String {
        format!("Share URL: {}\nGist: {}", self.viewer_url, self.gist_url)
    }
}

/// Share failures with compatibility error text.
#[derive(Debug, Error)]
pub enum ShareError {
    /// `gh` is absent from `PATH`.
    #[error("GitHub CLI (gh) is not installed. Install it from https://cli.github.com/")]
    GhNotInstalled,
    /// `gh auth status` failed.
    #[error("GitHub CLI is not logged in. Run 'gh auth login' first.")]
    GhNotLoggedIn,
    /// Session HTML generation failed.
    #[error("Failed to export session: {0}")]
    Export(#[from] ExportError),
    /// `gh gist create` failed.
    #[error("Failed to create gist: {0}")]
    GistCreateFailed(String),
    /// Successful command emitted no parseable URL.
    #[error("Failed to parse gist ID from gh output")]
    GistIdParseFailed,
    /// Operation was cancelled and the child was terminated by the runner.
    #[error("Share cancelled")]
    Cancelled,
    /// Temporary-file or command I/O failed.
    #[error("Failed to create gist: {0}")]
    Io(String),
}

impl From<CommandRunError> for ShareError {
    fn from(error: CommandRunError) -> Self {
        match error {
            CommandRunError::NotFound(_) => Self::GhNotInstalled,
            CommandRunError::Cancelled => Self::Cancelled,
            CommandRunError::Io(error) => Self::Io(error.to_string()),
        }
    }
}

/// Check `gh` availability and authentication through an injected runner.
///
/// # Errors
///
/// Distinguishes an absent binary from a nonzero auth status and cancellation.
pub async fn check_gh_auth_with(
    runner: &dyn CommandRunner,
    cancellation: &CancellationToken,
) -> Result<(), ShareError> {
    let arguments = ["auth".to_owned(), "status".to_owned()];
    let result = runner.run("gh", &arguments, cancellation).await?;
    if result.success() {
        Ok(())
    } else {
        Err(ShareError::GhNotLoggedIn)
    }
}

/// Check `gh` availability and authentication with the system runner.
///
/// # Errors
///
/// See [`check_gh_auth_with`].
pub async fn check_gh_auth(cancellation: &CancellationToken) -> Result<(), ShareError> {
    check_gh_auth_with(&SystemCommandRunner, cancellation).await
}

fn gist_url_from_stdout(stdout: &str) -> Option<&str> {
    stdout
        .lines()
        .rev()
        .map(str::trim)
        .find(|line| !line.is_empty())
}

/// Upload an existing HTML file as a secret gist through an injected runner.
///
/// # Errors
///
/// Returns exact authentication, command, cancellation, and URL parsing errors.
pub async fn share_html_file_with(
    html_path: &Path,
    runner: &dyn CommandRunner,
    cancellation: &CancellationToken,
) -> Result<ShareResult, ShareError> {
    check_gh_auth_with(runner, cancellation).await?;
    let arguments = vec![
        "gist".to_owned(),
        "create".to_owned(),
        "--public=false".to_owned(),
        html_path.to_string_lossy().into_owned(),
    ];
    let output = runner.run("gh", &arguments, cancellation).await?;
    if !output.success() {
        let message = output.stderr.trim();
        return Err(ShareError::GistCreateFailed(if message.is_empty() {
            "Unknown error".to_owned()
        } else {
            message.to_owned()
        }));
    }
    let gist_url = gist_url_from_stdout(&output.stdout)
        .ok_or(ShareError::GistIdParseFailed)?
        .to_owned();
    let gist_id = gist_url
        .rsplit('/')
        .next()
        .filter(|segment| !segment.is_empty())
        .ok_or(ShareError::GistIdParseFailed)?;
    Ok(ShareResult {
        viewer_url: get_share_viewer_url(gist_id),
        gist_url,
    })
}

/// Upload an existing HTML file with the system runner.
///
/// # Errors
///
/// See [`share_html_file_with`].
pub async fn share_html_file(
    html_path: &Path,
    cancellation: &CancellationToken,
) -> Result<ShareResult, ShareError> {
    share_html_file_with(html_path, &SystemCommandRunner, cancellation).await
}

struct TemporaryShareFile {
    directory: PathBuf,
    html: PathBuf,
}

impl TemporaryShareFile {
    fn create() -> Result<Self, ShareError> {
        let directory = std::env::temp_dir().join(format!("pi-share-{}", Uuid::new_v4()));
        std::fs::create_dir(&directory).map_err(|error| ShareError::Io(error.to_string()))?;
        Ok(Self {
            html: directory.join("session.html"),
            directory,
        })
    }
}

impl Drop for TemporaryShareFile {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.html);
        let _ = std::fs::remove_dir(&self.directory);
    }
}

/// Export a current session to temporary HTML and create a secret gist.
///
/// Authentication is checked before export. The temporary HTML and its unique
/// directory are removed on success, error, and cancellation.
///
/// # Errors
///
/// Returns export or share errors described by [`ShareError`].
pub async fn share_session_with(
    session: &SessionManager,
    state: Option<&SessionExportState>,
    runner: &dyn CommandRunner,
    cancellation: &CancellationToken,
) -> Result<ShareResult, ShareError> {
    check_gh_auth_with(runner, cancellation).await?;
    let temporary = TemporaryShareFile::create()?;
    export_session_to_html(
        session,
        state,
        ExportOptions {
            output_path: Some(temporary.html.clone()),
            ..ExportOptions::default()
        },
    )?;

    let arguments = vec![
        "gist".to_owned(),
        "create".to_owned(),
        "--public=false".to_owned(),
        temporary.html.to_string_lossy().into_owned(),
    ];
    let output = runner.run("gh", &arguments, cancellation).await?;
    if !output.success() {
        let message = output.stderr.trim();
        return Err(ShareError::GistCreateFailed(if message.is_empty() {
            "Unknown error".to_owned()
        } else {
            message.to_owned()
        }));
    }
    let gist_url = gist_url_from_stdout(&output.stdout)
        .ok_or(ShareError::GistIdParseFailed)?
        .to_owned();
    let gist_id = gist_url
        .rsplit('/')
        .next()
        .filter(|segment| !segment.is_empty())
        .ok_or(ShareError::GistIdParseFailed)?;
    Ok(ShareResult {
        viewer_url: get_share_viewer_url(gist_id),
        gist_url,
    })
}

/// Export and share a current session with the system command runner.
///
/// # Errors
///
/// See [`share_session_with`].
pub async fn share_session(
    session: &SessionManager,
    state: Option<&SessionExportState>,
    cancellation: &CancellationToken,
) -> Result<ShareResult, ShareError> {
    share_session_with(session, state, &SystemCommandRunner, cancellation).await
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;
    use tempfile::tempdir;

    type RecordedCall = (String, Vec<String>);
    #[derive(Clone, Debug)]
    enum FakeResponse {
        Output(CommandOutput),
        NotFound,
        WaitForCancellation,
    }

    #[derive(Clone, Default)]
    struct FakeRunner {
        responses: Arc<Mutex<VecDeque<FakeResponse>>>,
        calls: Arc<Mutex<Vec<RecordedCall>>>,
        gist_file_seen: Arc<Mutex<Option<PathBuf>>>,
    }

    impl FakeRunner {
        fn new(responses: impl IntoIterator<Item = FakeResponse>) -> Self {
            Self {
                responses: Arc::new(Mutex::new(responses.into_iter().collect())),
                ..Self::default()
            }
        }

        fn calls(&self) -> Vec<RecordedCall> {
            self.calls
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .clone()
        }

        fn gist_file(&self) -> Option<PathBuf> {
            self.gist_file_seen
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .clone()
        }
    }

    impl CommandRunner for FakeRunner {
        fn run<'a>(
            &'a self,
            program: &'a str,
            arguments: &'a [String],
            cancellation: &'a CancellationToken,
        ) -> BoxFuture<'a, Result<CommandOutput, CommandRunError>> {
            Box::pin(async move {
                self.calls
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .push((program.to_owned(), arguments.to_vec()));
                if arguments.first().is_some_and(|value| value == "gist")
                    && let Some(path) = arguments.get(3)
                {
                    *self
                        .gist_file_seen
                        .lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner) =
                        Some(PathBuf::from(path));
                    if !Path::new(path).exists() {
                        return Err(CommandRunError::Io(io::Error::new(
                            io::ErrorKind::NotFound,
                            "temporary HTML missing",
                        )));
                    }
                }
                let response = self
                    .responses
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .pop_front()
                    .ok_or_else(|| {
                        CommandRunError::Io(io::Error::new(
                            io::ErrorKind::UnexpectedEof,
                            "no fake response",
                        ))
                    })?;
                match response {
                    FakeResponse::Output(output) => Ok(output),
                    FakeResponse::NotFound => Err(CommandRunError::NotFound(program.to_owned())),
                    FakeResponse::WaitForCancellation => {
                        cancellation.cancelled().await;
                        Err(CommandRunError::Cancelled)
                    }
                }
            })
        }
    }

    fn ok(stdout: &str) -> FakeResponse {
        FakeResponse::Output(CommandOutput {
            status: Some(0),
            stdout: stdout.to_owned(),
            stderr: String::new(),
        })
    }

    fn persisted_session(root: &Path) -> Result<SessionManager, Box<dyn std::error::Error>> {
        let path = root.join("source.jsonl");
        std::fs::write(
            &path,
            concat!(
                "{\"type\":\"session\",\"version\":3,\"id\":\"share\",\"timestamp\":\"2026-01-01T00:00:00.000Z\",\"cwd\":\"/tmp\"}\n",
                "{\"type\":\"message\",\"id\":\"entry\",\"parentId\":null,\"timestamp\":\"2026-01-01T00:00:01.000Z\",\"message\":{\"role\":\"user\",\"content\":\"hello\",\"timestamp\":1}}\n"
            ),
        )?;
        Ok(SessionManager::open(
            &path.to_string_lossy(),
            Some(&root.to_string_lossy()),
            None,
        )?)
    }

    #[tokio::test]
    async fn distinguishes_missing_gh_and_failed_auth() -> Result<(), Box<dyn std::error::Error>> {
        let cancellation = CancellationToken::new();
        let missing = FakeRunner::new([FakeResponse::NotFound]);
        let error = check_gh_auth_with(&missing, &cancellation)
            .await
            .err()
            .ok_or("expected missing-gh error")?;
        assert_eq!(
            error.to_string(),
            "GitHub CLI (gh) is not installed. Install it from https://cli.github.com/"
        );

        let unauthenticated = FakeRunner::new([FakeResponse::Output(CommandOutput {
            status: Some(1),
            stdout: String::new(),
            stderr: "not logged in".to_owned(),
        })]);
        let error = check_gh_auth_with(&unauthenticated, &cancellation)
            .await
            .err()
            .ok_or("expected auth error")?;
        assert_eq!(
            error.to_string(),
            "GitHub CLI is not logged in. Run 'gh auth login' first."
        );
        Ok(())
    }

    #[tokio::test]
    async fn parses_last_url_and_uses_exact_private_gist_argv()
    -> Result<(), Box<dyn std::error::Error>> {
        let root = tempdir()?;
        let html = root.path().join("session.html");
        std::fs::write(&html, "html")?;
        let runner = FakeRunner::new([
            ok(""),
            ok("warning\nhttps://gist.github.com/user/gist-id\n"),
        ]);
        let result = share_html_file_with(&html, &runner, &CancellationToken::new()).await?;
        assert_eq!(result.viewer_url, "https://pi.dev/session/#gist-id");
        assert_eq!(result.gist_url, "https://gist.github.com/user/gist-id");
        assert_eq!(
            result.status_text(),
            concat!(
                "Share URL: https://pi.dev/session/#gist-id\n",
                "Gist: https://gist.github.com/user/gist-id"
            )
        );
        let calls = runner.calls();
        assert_eq!(calls[0].1, ["auth", "status"]);
        assert_eq!(calls[1].1[..3], ["gist", "create", "--public=false"]);
        assert_eq!(
            calls[1].1.get(3),
            Some(&html.to_string_lossy().into_owned())
        );
        Ok(())
    }

    #[tokio::test]
    async fn reports_no_url_and_gist_failure() -> Result<(), Box<dyn std::error::Error>> {
        let root = tempdir()?;
        let html = root.path().join("session.html");
        std::fs::write(&html, "html")?;
        let no_url = FakeRunner::new([ok(""), ok("\n")]);
        let error = share_html_file_with(&html, &no_url, &CancellationToken::new())
            .await
            .err()
            .ok_or("expected parse error")?;
        assert_eq!(error.to_string(), "Failed to parse gist ID from gh output");

        let failed = FakeRunner::new([
            ok(""),
            FakeResponse::Output(CommandOutput {
                status: Some(1),
                stdout: String::new(),
                stderr: "denied\n".to_owned(),
            }),
        ]);
        let error = share_html_file_with(&html, &failed, &CancellationToken::new())
            .await
            .err()
            .ok_or("expected gist error")?;
        assert_eq!(error.to_string(), "Failed to create gist: denied");
        Ok(())
    }

    #[tokio::test]
    async fn temporary_html_is_present_for_upload_and_cleaned_afterward()
    -> Result<(), Box<dyn std::error::Error>> {
        let root = tempdir()?;
        let session = persisted_session(root.path())?;
        let runner = FakeRunner::new([ok(""), ok("https://gist.github.com/user/cleanup-id\n")]);
        let result = share_session_with(&session, None, &runner, &CancellationToken::new()).await?;
        assert_eq!(result.viewer_url, "https://pi.dev/session/#cleanup-id");
        let temporary = runner.gist_file().ok_or("gist path not captured")?;
        assert!(!temporary.exists());
        assert!(!temporary.parent().is_some_and(Path::exists));
        Ok(())
    }

    #[tokio::test]
    async fn cancellation_propagates_and_cleans_temporary_html()
    -> Result<(), Box<dyn std::error::Error>> {
        let root = tempdir()?;
        let session = persisted_session(root.path())?;
        let runner = FakeRunner::new([ok(""), FakeResponse::WaitForCancellation]);
        let cancellation = CancellationToken::new();
        let cancel = cancellation.clone();
        let future = share_session_with(&session, None, &runner, &cancellation);
        tokio::pin!(future);
        let result = tokio::select! {
            biased;
            () = async { tokio::task::yield_now().await; cancel.cancel(); } => (&mut future).await,
            result = &mut future => result,
        };
        assert!(matches!(result, Err(ShareError::Cancelled)));
        let temporary = runner.gist_file().ok_or("gist path not captured")?;
        assert!(!temporary.exists());
        Ok(())
    }
}
