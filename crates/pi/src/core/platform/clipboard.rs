//! Cross-platform clipboard text and image I/O.
//!
//! Ports `.references/pi-2.0/packages/coding-agent/src/utils/{clipboard.ts,
//! clipboard-native.ts, clipboard-image.ts}`.
//!
//! The TypeScript reference uses the `@mariozechner/clipboard` native addon
//! as a fast path; this Rust port has no native addon and instead drives the
//! platform clipboard CLI tools directly. Those tools (`pbcopy`/`pbpaste`,
//! `clip`, `wl-copy`/`wl-paste`, `xclip`, `xsel`, `termux-clipboard-set`,
//! PowerShell) are the same ones the reference falls back to, so the
//! observable argv contract and the OSC 52 remote fallback are preserved
//! exactly and are unit-testable on any host.
//!
//! Image support reuses [`super::image::process_image`] so no external image
//! binaries are spawned.

use std::io;
use std::time::Duration;

use base64::Engine;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::process::{Child, Command};
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;
use uuid::Uuid;

use super::image::{convert_to_png, detect_supported_image_mime, extension_for_image_mime};

/// Maximum clipboard payload accepted from a command backend.
pub const MAX_CLIPBOARD_BYTES: usize = 50 * 1024 * 1024;

/// Maximum base64 length for an OSC 52 copy. Larger payloads are skipped to
/// avoid desynchronizing terminal rendering.
pub const MAX_OSC52_ENCODED_LENGTH: usize = 100_000;

/// Shell-tool timeout for clipboard helpers.
pub const CLIPBOARD_TIMEOUT: Duration = Duration::from_secs(5);

const CLIPBOARD_IMAGE_LIST_TIMEOUT: Duration = Duration::from_secs(1);

/// Platform discriminator selectable independently of the host for tests.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub enum ClipboardPlatform {
    /// macOS: `pbcopy` / `pbpaste`.
    Darwin,
    /// Windows: `clip` / PowerShell `Get-Clipboard`.
    Windows,
    /// Linux and other Unix: Wayland/X11/Termux tools.
    Unix,
}

impl ClipboardPlatform {
    /// Resolve the current host's platform.
    #[must_use]
    pub fn host() -> Self {
        #[cfg(target_os = "macos")]
        {
            Self::Darwin
        }
        #[cfg(target_os = "windows")]
        {
            Self::Windows
        }
        #[cfg(not(any(target_os = "macos", target_os = "windows")))]
        {
            Self::Unix
        }
    }
}

/// Read-only view of the environment used to make clipboard decisions.
///
/// Production reads `std::env`; tests inject values to exercise the remote,
/// Wayland, X11, and Termux branches deterministically.
pub trait ClipboardEnv: Send + Sync {
    /// Value of an environment variable, if set.
    fn get(&self, name: &str) -> Option<String>;
}

/// Production environment backed by `std::env::var`.
#[derive(Debug, Default)]
pub struct HostEnv;

impl ClipboardEnv for HostEnv {
    fn get(&self, name: &str) -> Option<String> {
        std::env::var(name).ok()
    }
}

/// Returns `true` when `env` indicates a Wayland session.
///
/// Matches `isWaylandSession`: `WAYLAND_DISPLAY` set or `XDG_SESSION_TYPE`
/// exactly `"wayland"`.
#[must_use]
pub fn is_wayland_session(env: &dyn ClipboardEnv) -> bool {
    env.get("WAYLAND_DISPLAY").is_some()
        || env.get("XDG_SESSION_TYPE").as_deref() == Some("wayland")
}

/// Returns `true` for an SSH or Mosh remote session, where OSC 52 is emitted
/// even after a native copy so the controlling terminal receives the text.
#[must_use]
pub fn is_remote_session(env: &dyn ClipboardEnv) -> bool {
    env.get("SSH_CONNECTION").is_some()
        || env.get("SSH_CLIENT").is_some()
        || env.get("MOSH_CONNECTION").is_some()
}

/// Errors returned by an asynchronous clipboard operation.
#[derive(Clone, Debug, Eq, PartialEq, thiserror::Error)]
pub enum ClipboardError {
    /// The caller cancelled the operation; any child was killed and reaped.
    #[error("Clipboard operation cancelled")]
    Cancelled,
    /// A command exceeded [`CLIPBOARD_TIMEOUT`].
    #[error("Clipboard command timed out: {program}")]
    TimedOut {
        /// Command whose deadline expired.
        program: String,
    },
    /// A command started but failed, exited unsuccessfully, or returned bad
    /// output.
    #[error("Clipboard command failed: {program}: {message}")]
    Process {
        /// Command whose operation failed.
        program: String,
        /// Stable failure detail for diagnostics and tests.
        message: String,
    },
    /// A command produced more than the clipboard output limit.
    #[error("Clipboard command output exceeded the limit: {program}")]
    OutputTooLarge {
        /// Command whose output exceeded the limit.
        program: String,
    },
    /// Every copy path (native, shell tool, OSC 52) failed.
    #[error("Failed to copy to clipboard")]
    Failed,
}

/// Result of a clipboard read. An empty clipboard is not an unavailable
/// backend, and a backend failure is not a successful empty read.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ClipboardReadResult<T> {
    /// The backend returned a value.
    Value(T),
    /// The backend successfully answered but had no value in the requested
    /// format.
    Empty,
    /// No selected backend could be started or no display/backend applies.
    Unavailable,
    /// A selected backend started but failed, timed out, was cancelled, or
    /// returned invalid/oversized data.
    Failed(ClipboardError),
}

/// Result of a clipboard write. OSC 52 is returned for the existing terminal
/// writer; this module never writes terminal bytes directly.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ClipboardCopyResult {
    /// A platform clipboard command accepted the text.
    Command,
    /// The caller must send this sequence through its sole terminal writer.
    Osc52(String),
}

/// A resolved clipboard write argv (program + args) with an optional fallback.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct WriteCommand {
    /// Program name.
    pub program: String,
    /// Argv excluding the program name.
    pub args: Vec<String>,
    /// Optional secondary argv tried when the primary is unavailable or fails.
    pub fallback: Option<(String, Vec<String>)>,
}

impl WriteCommand {
    fn new(program: &str, args: Vec<String>) -> Self {
        Self {
            program: program.to_owned(),
            args,
            fallback: None,
        }
    }
}

/// Selected write command argv for `platform`/`env`, or `None` when no shell
/// tool applies (forcing the OSC 52 / failure path).
///
/// This compatibility selector returns the first command in the same order as
/// [`clipboard_write_commands`]. Its X11 entry retains the `xsel` fallback.
#[must_use]
pub fn clipboard_write_command(
    platform: ClipboardPlatform,
    env: &dyn ClipboardEnv,
) -> Option<WriteCommand> {
    clipboard_write_commands(platform, env).into_iter().next()
}

/// Resolve every command attempted by the text writer, in reference order.
///
/// Linux intentionally retains all applicable branches: Termux, Wayland, and
/// X11 may coexist, and a failed earlier backend must not prevent the later
/// fallback.
#[must_use]
pub fn clipboard_write_commands(
    platform: ClipboardPlatform,
    env: &dyn ClipboardEnv,
) -> Vec<WriteCommand> {
    match platform {
        ClipboardPlatform::Darwin => vec![WriteCommand::new("pbcopy", vec![])],
        ClipboardPlatform::Windows => vec![WriteCommand::new("clip", vec![])],
        ClipboardPlatform::Unix => {
            let mut commands = Vec::new();
            if env.get("TERMUX_VERSION").is_some() {
                commands.push(WriteCommand::new("termux-clipboard-set", vec![]));
            }
            if env.get("WAYLAND_DISPLAY").is_some() {
                commands.push(WriteCommand::new("wl-copy", vec![]));
            }
            if env.get("DISPLAY").is_some() {
                let mut x11 = WriteCommand::new(
                    "xclip",
                    vec!["-selection".to_owned(), "clipboard".to_owned()],
                );
                x11.fallback = Some((
                    "xsel".to_owned(),
                    vec!["--clipboard".to_owned(), "--input".to_owned()],
                ));
                commands.push(x11);
            }
            commands
        }
    }
}

/// Encode `text` as an OSC 52 sequence, or `None` when the base64 form exceeds
/// [`MAX_OSC52_ENCODED_LENGTH`].
#[must_use]
pub fn osc52_encode(text: &str) -> Option<String> {
    let encoded = base64::engine::general_purpose::STANDARD.encode(text);
    if encoded.len() > MAX_OSC52_ENCODED_LENGTH {
        return None;
    }
    Some(format!("\x1b]52;c;{encoded}\x07"))
}

#[derive(Debug)]
enum ProcessResult {
    Success(Vec<u8>),
    Unavailable,
    Failed(ClipboardError),
}

async fn stop_child(child: &mut Child) {
    let _ = child.kill().await;
    let _ = child.wait().await;
}

async fn read_child_output(
    mut stdout: tokio::process::ChildStdout,
    program: String,
) -> Result<Vec<u8>, ClipboardError> {
    let mut output = Vec::new();
    let mut buffer = [0_u8; 8192];
    loop {
        let read = stdout
            .read(&mut buffer)
            .await
            .map_err(|error| ClipboardError::Process {
                program: program.clone(),
                message: error.to_string(),
            })?;
        if read == 0 {
            break;
        }
        if read > MAX_CLIPBOARD_BYTES.saturating_sub(output.len()) {
            return Err(ClipboardError::OutputTooLarge { program });
        }
        output.extend_from_slice(&buffer[..read]);
    }
    Ok(output)
}
async fn cancel_tasks(
    writer: Option<JoinHandle<Result<(), io::Error>>>,
    output: Option<JoinHandle<Result<Vec<u8>, ClipboardError>>>,
) {
    if let Some(writer) = writer {
        writer.abort();
        let _ = writer.await;
    }
    if let Some(output) = output {
        output.abort();
        let _ = output.await;
    }
}

/// Spawn one clipboard helper with bounded output, timeout, cancellation, and
/// child reaping. Missing executables are unavailable; started-but-failed
/// commands remain failures for the caller to report after fallback ordering.
async fn run_process(
    program: &str,
    args: &[String],
    input: Option<&[u8]>,
    cancel: &CancellationToken,
    timeout: Duration,
) -> ProcessResult {
    if cancel.is_cancelled() {
        return ProcessResult::Failed(ClipboardError::Cancelled);
    }

    let mut command = Command::new(program);
    command
        .args(args)
        .stdin(if input.is_some() {
            std::process::Stdio::piped()
        } else {
            std::process::Stdio::null()
        })
        .stdout(if input.is_some() {
            std::process::Stdio::null()
        } else {
            std::process::Stdio::piped()
        })
        .stderr(std::process::Stdio::null())
        .kill_on_drop(true);
    let mut child = match command.spawn() {
        Ok(child) => child,
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            return ProcessResult::Unavailable;
        }
        Err(error) => {
            return ProcessResult::Failed(ClipboardError::Process {
                program: program.to_owned(),
                message: error.to_string(),
            });
        }
    };

    let mut writer = input.map(|bytes| {
        let bytes = bytes.to_vec();
        let Some(mut stdin) = child.stdin.take() else {
            return tokio::spawn(async { Ok::<(), io::Error>(()) });
        };
        tokio::spawn(async move {
            // A writer may exit before consuming all input; the exit status
            // remains authoritative, matching the reference command helper.
            let _ = stdin.write_all(&bytes).await;
            drop(stdin);
            Ok::<(), io::Error>(())
        })
    });
    let mut output_task = child
        .stdout
        .take()
        .map(|stdout| tokio::spawn(read_child_output(stdout, program.to_owned())));

    let mut deadline = Box::pin(tokio::time::sleep(timeout));
    let status = tokio::select! {
        biased;
        () = cancel.cancelled() => {
            stop_child(&mut child).await;
            cancel_tasks(writer.take(), output_task.take()).await;
            return ProcessResult::Failed(ClipboardError::Cancelled);
        }
        () = &mut deadline => {
            stop_child(&mut child).await;
            cancel_tasks(writer.take(), output_task.take()).await;
            return ProcessResult::Failed(ClipboardError::TimedOut { program: program.to_owned() });
        }
        status = child.wait() => status,
    };

    let status = match status {
        Ok(status) => status,
        Err(error) => {
            stop_child(&mut child).await;
            cancel_tasks(writer.take(), output_task.take()).await;
            return ProcessResult::Failed(ClipboardError::Process {
                program: program.to_owned(),
                message: error.to_string(),
            });
        }
    };
    if let Some(writer) = writer {
        writer.abort();
        let _ = writer.await;
    }
    let output = if let Some(task) = output_task.take() {
        match task.await {
            Ok(Ok(output)) => output,
            Ok(Err(error)) => return ProcessResult::Failed(error),
            Err(error) => {
                return ProcessResult::Failed(ClipboardError::Process {
                    program: program.to_owned(),
                    message: error.to_string(),
                });
            }
        }
    } else {
        Vec::new()
    };
    if status.success() {
        ProcessResult::Success(output)
    } else {
        ProcessResult::Failed(ClipboardError::Process {
            program: program.to_owned(),
            message: format!("exit status {status}"),
        })
    }
}

fn merge_process_results(first: ProcessResult, second: ProcessResult) -> ProcessResult {
    match second {
        ProcessResult::Success(output) => ProcessResult::Success(output),
        ProcessResult::Failed(error) => ProcessResult::Failed(error),
        ProcessResult::Unavailable => first,
    }
}

async fn run_write_command(
    cmd: &WriteCommand,
    text: &str,
    cancel: &CancellationToken,
) -> ProcessResult {
    let first = run_process(
        &cmd.program,
        &cmd.args,
        Some(text.as_bytes()),
        cancel,
        CLIPBOARD_TIMEOUT,
    )
    .await;
    if matches!(
        &first,
        ProcessResult::Success(_) | ProcessResult::Failed(ClipboardError::Cancelled)
    ) {
        return first;
    }
    let Some((program, args)) = &cmd.fallback else {
        return first;
    };
    let second = run_process(
        program,
        args,
        Some(text.as_bytes()),
        cancel,
        CLIPBOARD_TIMEOUT,
    )
    .await;
    merge_process_results(first, second)
}

/// Copy `text` without blocking the product runtime.
///
/// Platform commands run before OSC 52. Remote sessions return an OSC 52
/// operation even after a successful command so the controlling terminal also
/// receives the text. The returned sequence must be written through the
/// product's sole terminal writer; this module never writes stdout.
///
/// # Errors
///
/// Returns [`ClipboardError::Cancelled`] or [`ClipboardError::TimedOut`] when
/// the selected operation is interrupted, and preserves the final command
/// failure when OSC 52 is unavailable.
pub async fn copy_to_clipboard_with(
    text: &str,
    platform: ClipboardPlatform,
    env: &dyn ClipboardEnv,
    cancel: &CancellationToken,
) -> Result<ClipboardCopyResult, ClipboardError> {
    let mut last_failure = None;
    for command in clipboard_write_commands(platform, env) {
        match run_write_command(&command, text, cancel).await {
            ProcessResult::Success(_) => {
                if cancel.is_cancelled() {
                    return Err(ClipboardError::Cancelled);
                }
                if is_remote_session(env) {
                    return osc52_encode(text)
                        .map(ClipboardCopyResult::Osc52)
                        .ok_or(ClipboardError::Failed);
                }
                return Ok(ClipboardCopyResult::Command);
            }
            ProcessResult::Unavailable => {}
            ProcessResult::Failed(error @ ClipboardError::Cancelled) => return Err(error),
            ProcessResult::Failed(error) => last_failure = Some(error),
        }
    }

    if cancel.is_cancelled() {
        return Err(ClipboardError::Cancelled);
    }
    if let Some(sequence) = osc52_encode(text) {
        return Ok(ClipboardCopyResult::Osc52(sequence));
    }
    Err(last_failure.unwrap_or(ClipboardError::Failed))
}

/// A resolved clipboard read argv with an optional fallback.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ReadCommand {
    /// Program name.
    pub program: String,
    /// Argv excluding the program name.
    pub args: Vec<String>,
    /// Optional secondary argv when the primary is unavailable.
    pub fallback: Option<(String, Vec<String>)>,
}

impl ReadCommand {
    fn new(program: &str, args: Vec<String>) -> Self {
        Self {
            program: program.to_owned(),
            args,
            fallback: None,
        }
    }
}

/// Selected read command argv for `platform`/`env`, or `None`.
///
/// This compatibility selector returns the first command in the same order as
/// [`clipboard_read_commands`]. Its X11 entry retains the `xsel` fallback.
#[must_use]
pub fn clipboard_read_command(
    platform: ClipboardPlatform,
    env: &dyn ClipboardEnv,
) -> Option<ReadCommand> {
    clipboard_read_commands(platform, env).into_iter().next()
}

/// Resolve every command attempted by the text reader, in reference order.
#[must_use]
pub fn clipboard_read_commands(
    platform: ClipboardPlatform,
    env: &dyn ClipboardEnv,
) -> Vec<ReadCommand> {
    match platform {
        ClipboardPlatform::Darwin => vec![ReadCommand::new("pbpaste", vec![])],
        ClipboardPlatform::Windows => vec![ReadCommand::new(
            "powershell",
            vec![
                "-NoProfile".to_owned(),
                "-Command".to_owned(),
                "Get-Clipboard".to_owned(),
            ],
        )],
        ClipboardPlatform::Unix => {
            let mut commands = Vec::new();
            if env.get("TERMUX_VERSION").is_some() {
                commands.push(ReadCommand::new("termux-clipboard-get", vec![]));
            }
            if env.get("WAYLAND_DISPLAY").is_some() {
                commands.push(ReadCommand::new(
                    "wl-paste",
                    vec!["--no-newline".to_owned(), "--type".to_owned(), "text".to_owned()],
                ));
            }
            if env.get("DISPLAY").is_some() {
                let mut x11 = ReadCommand::new(
                    "xclip",
                    vec![
                        "-selection".to_owned(),
                        "clipboard".to_owned(),
                        "-out".to_owned(),
                    ],
                );
                x11.fallback = Some((
                    "xsel".to_owned(),
                    vec!["--clipboard".to_owned(), "--output".to_owned()],
                ));
                commands.push(x11);
            }
            commands
        }
    }
}

async fn run_read_command(
    cmd: &ReadCommand,
    cancel: &CancellationToken,
    timeout: Duration,
) -> ProcessResult {
    let first = run_process(&cmd.program, &cmd.args, None, cancel, timeout).await;
    if matches!(&first, ProcessResult::Success(_) | ProcessResult::Failed(ClipboardError::Cancelled)) {
        return first;
    }
    let Some((program, args)) = &cmd.fallback else {
        return first;
    };
    let second = run_process(program, args, None, cancel, timeout).await;
    merge_process_results(first, second)
}

/// Read plain text from the host clipboard with a cancellation-aware async
/// fallback chain.
#[must_use]
pub async fn read_clipboard_text(cancel: &CancellationToken) -> ClipboardReadResult<String> {
    let env = HostEnv;
    read_clipboard_text_with(ClipboardPlatform::host(), &env, cancel).await
}

/// Read plain text with an explicit platform/env and cancellation token.
///
/// A successful empty command result stops the chain. This prevents an empty
/// Wayland or X11 selection from falling through to stale clipboard contents.
pub async fn read_clipboard_text_with(
    platform: ClipboardPlatform,
    env: &dyn ClipboardEnv,
    cancel: &CancellationToken,
) -> ClipboardReadResult<String> {
    if cancel.is_cancelled() {
        return ClipboardReadResult::Failed(ClipboardError::Cancelled);
    }
    let mut failure = None;
    for command in clipboard_read_commands(platform, env) {
        match run_read_command(&command, cancel, CLIPBOARD_TIMEOUT).await {
            ProcessResult::Success(bytes) => {
                if bytes.is_empty() {
                    return ClipboardReadResult::Empty;
                }
                return ClipboardReadResult::Value(String::from_utf8_lossy(&bytes).into_owned());
            }
            ProcessResult::Unavailable => {}
            ProcessResult::Failed(error @ ClipboardError::Cancelled) => {
                return ClipboardReadResult::Failed(error);
            }
            ProcessResult::Failed(error) => failure = Some(error),
        }
    }
    failure.map_or(ClipboardReadResult::Unavailable, ClipboardReadResult::Failed)
}

/// Returns `true` on WSL using `WSL_DISTRO_NAME`, `WSLENV`, or `/proc/version`.
pub fn is_wsl(env: &dyn ClipboardEnv) -> bool {
    if env.get("WSL_DISTRO_NAME").is_some() || env.get("WSLENV").is_some() {
        return true;
    }
    std::fs::read_to_string("/proc/version").is_ok_and(|release| {
        release.contains("microsoft") || release.to_ascii_lowercase().contains("wsl")
    })
}

/// Convert unsupported image bytes to PNG for clipboard consumers.
///
/// Returns the supported `(bytes, mime)` unchanged, or a PNG conversion.
/// Returns `None` when the bytes are neither recognizable nor convertible.
#[must_use]
pub fn maybe_convert_to_png(bytes: &[u8], mime: &str) -> Option<(Vec<u8>, String)> {
    if let Some(kind) = detect_supported_image_mime(bytes)
        && matches!(
            kind.mime(),
            "image/png" | "image/jpeg" | "image/gif" | "image/webp"
        )
    {
        return Some((bytes.to_vec(), kind.mime().to_owned()));
    }
    let base = base_mime(mime);
    if matches!(
        base.as_str(),
        "image/png" | "image/jpeg" | "image/gif" | "image/webp"
    ) {
        return Some((bytes.to_vec(), base));
    }
    convert_to_png(bytes).map(|png| (png, "image/png".to_owned()))
}

fn base_mime(mime: &str) -> String {
    mime.split(';')
        .next()
        .unwrap_or(mime)
        .trim()
        .to_ascii_lowercase()
}

fn image_conversion_failure() -> ClipboardError {
    ClipboardError::Process {
        program: "clipboard image conversion".to_owned(),
        message: "unsupported image format".to_owned(),
    }
}

fn finalize_image(
    result: ClipboardReadResult<ClipboardImage>,
) -> ClipboardReadResult<ClipboardImage> {
    let ClipboardReadResult::Value(image) = result else {
        return result;
    };
    let Some((bytes, mime)) = maybe_convert_to_png(&image.bytes, &image.mime) else {
        return ClipboardReadResult::Failed(image_conversion_failure());
    };
    ClipboardReadResult::Value(ClipboardImage { bytes, mime })
}

async fn read_wl_paste_image(cancel: &CancellationToken) -> ClipboardReadResult<ClipboardImage> {
    let list = run_process(
        "wl-paste",
        &["--list-types".to_owned()],
        None,
        cancel,
        CLIPBOARD_IMAGE_LIST_TIMEOUT,
    )
    .await;
    let types = match list {
        ProcessResult::Success(bytes) => String::from_utf8_lossy(&bytes).into_owned(),
        ProcessResult::Unavailable => return ClipboardReadResult::Unavailable,
        ProcessResult::Failed(error) => return ClipboardReadResult::Failed(error),
    };
    let Some(selected) = select_preferred_image_mime(&types) else {
        return ClipboardReadResult::Empty;
    };
    let data = run_process(
        "wl-paste",
        &[
            "--type".to_owned(),
            selected.clone(),
            "--no-newline".to_owned(),
        ],
        None,
        cancel,
        CLIPBOARD_TIMEOUT,
    )
    .await;
    match data {
        ProcessResult::Success(bytes) if bytes.is_empty() => ClipboardReadResult::Empty,
        ProcessResult::Success(bytes) => ClipboardReadResult::Value(ClipboardImage {
            bytes,
            mime: base_mime(&selected),
        }),
        ProcessResult::Unavailable => ClipboardReadResult::Unavailable,
        ProcessResult::Failed(error) => ClipboardReadResult::Failed(error),
    }
}

async fn read_xclip_image(cancel: &CancellationToken) -> ClipboardReadResult<ClipboardImage> {
    let targets = run_process(
        "xclip",
        &[
            "-selection".to_owned(),
            "clipboard".to_owned(),
            "-t".to_owned(),
            "TARGETS".to_owned(),
            "-o".to_owned(),
        ],
        None,
        cancel,
        CLIPBOARD_IMAGE_LIST_TIMEOUT,
    )
    .await;

    let mut failure = None;
    let mut saw_empty = false;
    let mut candidate_types = Vec::new();
    match targets {
        ProcessResult::Success(bytes) => {
            candidate_types = String::from_utf8_lossy(&bytes)
                .lines()
                .map(str::trim)
                .filter(|line| !line.is_empty())
                .map(str::to_owned)
                .collect();
            if !candidate_types.is_empty()
                && select_preferred_image_mime(&candidate_types.join("\n")).is_none()
            {
                return ClipboardReadResult::Empty;
            }
        }
        ProcessResult::Unavailable => {}
        ProcessResult::Failed(error @ ClipboardError::Cancelled) => {
            return ClipboardReadResult::Failed(error);
        }
        ProcessResult::Failed(error) => failure = Some(error),
    }

    let preferred = select_preferred_image_mime(&candidate_types.join("\n"));
    let mut try_types = Vec::new();
    if let Some(preferred) = preferred {
        try_types.push(preferred);
    }
    for supported in ["image/png", "image/jpeg", "image/webp", "image/gif"] {
        if !try_types.iter().any(|mime| mime == supported) {
            try_types.push(supported.to_owned());
        }
    }

    for mime in try_types {
        let data = run_process(
            "xclip",
            &[
                "-selection".to_owned(),
                "clipboard".to_owned(),
                "-t".to_owned(),
                mime.clone(),
                "-o".to_owned(),
            ],
            None,
            cancel,
            CLIPBOARD_TIMEOUT,
        )
        .await;
        match data {
            ProcessResult::Success(bytes) if bytes.is_empty() => saw_empty = true,
            ProcessResult::Success(bytes) => {
                return ClipboardReadResult::Value(ClipboardImage {
                    bytes,
                    mime: base_mime(&mime),
                });
            }
            ProcessResult::Unavailable => {}
            ProcessResult::Failed(error @ ClipboardError::Cancelled) => {
                return ClipboardReadResult::Failed(error);
            }
            ProcessResult::Failed(error) => failure = Some(error),
        }
    }
    if let Some(error) = failure {
        ClipboardReadResult::Failed(error)
    } else if saw_empty {
        ClipboardReadResult::Empty
    } else {
        ClipboardReadResult::Unavailable
    }
}

/// On WSL, the Linux clipboard often does not receive image data copied in
/// Windows (for example, Win+Shift+S). PowerShell can reach the Windows
/// clipboard directly, so save a PNG to a temporary file and read it back.
async fn read_clipboard_image_via_powershell(
    cancel: &CancellationToken,
) -> ClipboardReadResult<ClipboardImage> {
    let tmp_file = std::env::temp_dir().join(format!("pi-wsl-clip-{}.png", Uuid::new_v4()));
    let result = async {
        let path = tmp_file.to_str().ok_or_else(|| {
            ClipboardReadResult::Failed(ClipboardError::Process {
                program: "wslpath".to_owned(),
                message: "temporary path is not valid UTF-8".to_owned(),
            })
        })?;
        let path_result = run_process(
            "wslpath",
            &["-w".to_owned(), path.to_owned()],
            None,
            cancel,
            CLIPBOARD_IMAGE_LIST_TIMEOUT,
        )
        .await;
        let win_path = match path_result {
            ProcessResult::Success(bytes) => String::from_utf8_lossy(&bytes).trim().to_owned(),
            ProcessResult::Unavailable => return Ok(ClipboardReadResult::Unavailable),
            ProcessResult::Failed(error) => return Ok(ClipboardReadResult::Failed(error)),
        };
        if win_path.is_empty() {
            return Ok(ClipboardReadResult::Empty);
        }

        let quoted = win_path.replace('\'', "''");
        let script = format!(
            "Add-Type -AssemblyName System.Windows.Forms; \
             Add-Type -AssemblyName System.Drawing; \
             $path = '{quoted}'; \
             $img = [System.Windows.Forms.Clipboard]::GetImage(); \
             if ($img) {{ $img.Save($path, [System.Drawing.Imaging.ImageFormat]::Png); Write-Output 'ok' }} else {{ Write-Output 'empty' }}"
        );
        let powershell = run_process(
            "powershell.exe",
            &[
                "-NoProfile".to_owned(),
                "-Command".to_owned(),
                script,
            ],
            None,
            cancel,
            CLIPBOARD_TIMEOUT,
        )
        .await;
        let output = match powershell {
            ProcessResult::Success(bytes) => String::from_utf8_lossy(&bytes).trim().to_owned(),
            ProcessResult::Unavailable => return Ok(ClipboardReadResult::Unavailable),
            ProcessResult::Failed(error) => return Ok(ClipboardReadResult::Failed(error)),
        };
        if output == "empty" {
            return Ok(ClipboardReadResult::Empty);
        }
        if output != "ok" {
            return Ok(ClipboardReadResult::Failed(ClipboardError::Process {
                program: "powershell.exe".to_owned(),
                message: "clipboard image command returned an unknown result".to_owned(),
            }));
        }
        let bytes = tokio::fs::read(&tmp_file).await.map_err(|error| {
            ClipboardReadResult::Failed(ClipboardError::Process {
                program: "powershell.exe".to_owned(),
                message: error.to_string(),
            })
        })?;
        if bytes.is_empty() {
            return Ok(ClipboardReadResult::Empty);
        }
        Ok(ClipboardReadResult::Value(ClipboardImage {
            bytes,
            mime: "image/png".to_owned(),
        }))
    }
    .await;
    let _ = tokio::fs::remove_file(&tmp_file).await;
    match result {
        Ok(value) | Err(value) => value,
    }
}

/// Read an image from the host clipboard, converting unsupported formats to
/// PNG. Linux command fallbacks preserve the reference's empty/unavailable
/// distinction; native Rust APIs are intentionally not guessed or installed.
#[must_use]
pub async fn read_clipboard_image(
    cancel: &CancellationToken,
) -> ClipboardReadResult<ClipboardImage> {
    let env = HostEnv;
    read_clipboard_image_with(ClipboardPlatform::host(), &env, cancel).await
}

/// Read an image with an explicit platform/env and cancellation token.
pub async fn read_clipboard_image_with(
    platform: ClipboardPlatform,
    env: &dyn ClipboardEnv,
    cancel: &CancellationToken,
) -> ClipboardReadResult<ClipboardImage> {
    if cancel.is_cancelled() {
        return ClipboardReadResult::Failed(ClipboardError::Cancelled);
    }
    if env.get("TERMUX_VERSION").is_some() {
        return ClipboardReadResult::Empty;
    }
    if !matches!(platform, ClipboardPlatform::Unix) {
        return ClipboardReadResult::Unavailable;
    }

    let wayland = env.get("WAYLAND_DISPLAY").is_some();
    let wsl = is_wsl(env);
    let mut image = if wayland || wsl {
        read_wl_paste_image(cancel).await
    } else {
        ClipboardReadResult::Unavailable
    };
    if cancel.is_cancelled() {
        return ClipboardReadResult::Failed(ClipboardError::Cancelled);
    }

    // A failed/unavailable Wayland backend may fall through to X11. An empty
    // Wayland selection must not expose stale X11 contents.
    if matches!(
        &image,
        ClipboardReadResult::Unavailable | ClipboardReadResult::Failed(_)
    ) {
        image = read_xclip_image(cancel).await;
    }
    if cancel.is_cancelled() {
        return ClipboardReadResult::Failed(ClipboardError::Cancelled);
    }
    if wsl && !matches!(&image, ClipboardReadResult::Value(_)) {
        let powershell = read_clipboard_image_via_powershell(cancel).await;
        if matches!(
            &powershell,
            ClipboardReadResult::Value(_) | ClipboardReadResult::Empty
        ) || matches!(&image, ClipboardReadResult::Unavailable)
        {
            image = powershell;
        }
    }
    if cancel.is_cancelled() {
        return ClipboardReadResult::Failed(ClipboardError::Cancelled);
    }
    finalize_image(image)
}

fn select_preferred_image_mime(types_output: &str) -> Option<String> {
    let normalized: Vec<String> = types_output
        .lines()
        .map(|line| line.trim().to_ascii_lowercase())
        .filter(|line| !line.is_empty())
        .collect();
    for preferred in ["image/png", "image/jpeg", "image/webp", "image/gif"] {
        if let Some(matched) = normalized.iter().find(|t| t.as_str() == preferred) {
            return Some(matched.clone());
        }
    }
    normalized.into_iter().find(|t| t.starts_with("image/"))
}

/// Extension for an image MIME, re-exported from the image module.
#[must_use]
pub fn extension_for_image_mime_str(mime: &str) -> Option<&'static str> {
    extension_for_image_mime(mime)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;
    use std::io;

    type TestResult = Result<(), Box<dyn std::error::Error>>;

    fn required<T>(value: Option<T>, context: &'static str) -> io::Result<T> {
        value.ok_or_else(|| io::Error::other(context))
    }

    #[derive(Default)]
    struct MapEnv {
        vars: HashMap<String, String>,
    }

    impl MapEnv {
        fn set(mut self, k: &str, v: &str) -> Self {
            self.vars.insert(k.to_owned(), v.to_owned());
            self
        }
    }

    impl ClipboardEnv for MapEnv {
        fn get(&self, name: &str) -> Option<String> {
            self.vars.get(name).cloned()
        }
    }

    #[test]
    fn darwin_write_is_pbcopy() -> TestResult {
        let env = MapEnv::default();
        let cmd = required(
            clipboard_write_command(ClipboardPlatform::Darwin, &env),
            "Darwin write command",
        )?;
        assert_eq!(cmd.program, "pbcopy");
        assert!(cmd.args.is_empty());
        Ok(())
    }

    #[test]
    fn windows_write_is_clip() -> TestResult {
        let env = MapEnv::default();
        let cmd = required(
            clipboard_write_command(ClipboardPlatform::Windows, &env),
            "Windows write command",
        )?;
        assert_eq!(cmd.program, "clip");
        Ok(())
    }

    #[test]
    fn unix_termux_wins_when_termux_version_set() -> TestResult {
        let env = MapEnv::default().set("TERMUX_VERSION", "1.0");
        let cmd = required(
            clipboard_write_command(ClipboardPlatform::Unix, &env),
            "Termux write command",
        )?;
        assert_eq!(cmd.program, "termux-clipboard-set");
        Ok(())
    }

    #[test]
    fn unix_wayland_when_wayland_display_and_session() -> TestResult {
        let env = MapEnv::default()
            .set("WAYLAND_DISPLAY", "wayland-0")
            .set("XDG_SESSION_TYPE", "wayland");
        let cmd = required(
            clipboard_write_command(ClipboardPlatform::Unix, &env),
            "Wayland write command",
        )?;
        assert_eq!(cmd.program, "wl-copy");
        Ok(())
    }

    #[test]
    fn unix_write_commands_keep_reference_order() {
        let env = MapEnv::default()
            .set("TERMUX_VERSION", "1.0")
            .set("WAYLAND_DISPLAY", "wayland-0")
            .set("DISPLAY", ":0");
        let commands = clipboard_write_commands(ClipboardPlatform::Unix, &env);
        assert_eq!(
            commands
                .iter()
                .map(|command| command.program.as_str())
                .collect::<Vec<_>>(),
            vec!["termux-clipboard-set", "wl-copy", "xclip"]
        );
    }

    #[test]
    fn unix_read_commands_keep_reference_order() {
        let env = MapEnv::default()
            .set("TERMUX_VERSION", "1.0")
            .set("WAYLAND_DISPLAY", "wayland-0")
            .set("DISPLAY", ":0");
        let commands = clipboard_read_commands(ClipboardPlatform::Unix, &env);
        assert_eq!(
            commands
                .iter()
                .map(|command| command.program.as_str())
                .collect::<Vec<_>>(),
            vec!["termux-clipboard-get", "wl-paste", "xclip"]
        );
    }

    #[test]
    fn unix_xclip_when_display_only_with_xsel_fallback() -> TestResult {
        let env = MapEnv::default().set("DISPLAY", ":0");
        let cmd = required(
            clipboard_write_command(ClipboardPlatform::Unix, &env),
            "X11 write command",
        )?;
        assert_eq!(cmd.program, "xclip");
        assert_eq!(cmd.args, vec!["-selection", "clipboard"]);
        let (fallback_prog, fallback_args) = required(cmd.fallback, "X11 write fallback")?;
        assert_eq!(fallback_prog, "xsel");
        assert_eq!(fallback_args, vec!["--clipboard", "--input"]);
        Ok(())
    }

    #[test]
    fn unix_no_display_returns_none_forcing_osc52() {
        let env = MapEnv::default();
        assert!(clipboard_write_command(ClipboardPlatform::Unix, &env).is_none());
    }

    #[test]
    fn osc52_encodes_small_text() -> TestResult {
        let seq = required(osc52_encode("hi"), "OSC 52 sequence")?;
        assert!(seq.starts_with("\x1b]52;c;"));
        assert!(seq.ends_with('\x07'));
        Ok(())
    }

    #[test]
    fn osc52_rejects_oversized_payload() {
        let big = "a".repeat(MAX_OSC52_ENCODED_LENGTH * 3 / 4 + 1);
        assert!(osc52_encode(&big).is_none());
    }

    #[tokio::test]
    async fn osc52_fallback_returns_sequence_when_no_tool_applies() -> TestResult {
        let env = MapEnv::default();
        let cancel = CancellationToken::new();
        let result =
            copy_to_clipboard_with("hi", ClipboardPlatform::Unix, &env, &cancel).await?;
        assert_eq!(
            result,
            ClipboardCopyResult::Osc52(required(osc52_encode("hi"), "OSC 52 sequence")?)
        );
        Ok(())
    }

    #[tokio::test]
    async fn osc52_returns_in_remote_session() -> TestResult {
        let env = MapEnv::default().set("SSH_CONNECTION", "1.2.3.4");
        let cancel = CancellationToken::new();
        let result =
            copy_to_clipboard_with("hi", ClipboardPlatform::Unix, &env, &cancel).await?;
        assert!(matches!(result, ClipboardCopyResult::Osc52(_)));
        Ok(())
    }

    #[tokio::test]
    async fn oversize_without_tool_errors_without_operation() {
        let env = MapEnv::default();
        let cancel = CancellationToken::new();
        let result = copy_to_clipboard_with(
            &"a".repeat(MAX_OSC52_ENCODED_LENGTH * 3 / 4 + 1),
            ClipboardPlatform::Unix,
            &env,
            &cancel,
        )
        .await;
        assert!(matches!(result, Err(ClipboardError::Failed)));
    }
    #[tokio::test]
    async fn pre_cancelled_operations_report_cancellation() {
        let env = MapEnv::default();
        let cancel = CancellationToken::new();
        cancel.cancel();
        assert!(matches!(
            copy_to_clipboard_with("hi", ClipboardPlatform::Unix, &env, &cancel).await,
            Err(ClipboardError::Cancelled)
        ));
        assert!(matches!(
            read_clipboard_text_with(ClipboardPlatform::Unix, &env, &cancel).await,
            ClipboardReadResult::Failed(ClipboardError::Cancelled)
        ));
        assert!(matches!(
            read_clipboard_image_with(ClipboardPlatform::Unix, &env, &cancel).await,
            ClipboardReadResult::Failed(ClipboardError::Cancelled)
        ));
    }

    #[test]
    fn is_remote_detects_ssh_and_mosh() {
        let ssh = MapEnv::default().set("SSH_CONNECTION", "1.2.3.4");
        assert!(is_remote_session(&ssh));
        let mosh = MapEnv::default().set("MOSH_CONNECTION", "1");
        assert!(is_remote_session(&mosh));
        let local = MapEnv::default();
        assert!(!is_remote_session(&local));
    }

    #[test]
    fn read_argv_matches_platform() -> TestResult {
        let env = MapEnv::default();
        let darwin = required(
            clipboard_read_command(ClipboardPlatform::Darwin, &env),
            "Darwin read command",
        )?;
        assert_eq!(darwin.program, "pbpaste");
        let win = required(
            clipboard_read_command(ClipboardPlatform::Windows, &env),
            "Windows read command",
        )?;
        assert_eq!(win.program, "powershell");
        assert_eq!(win.args, vec!["-NoProfile", "-Command", "Get-Clipboard"]);
        Ok(())
    }

    #[test]
    fn read_wayland_is_wl_paste_text_no_newline() -> TestResult {
        let env = MapEnv::default()
            .set("WAYLAND_DISPLAY", "wayland-0")
            .set("XDG_SESSION_TYPE", "wayland");
        let cmd = required(
            clipboard_read_command(ClipboardPlatform::Unix, &env),
            "Wayland read command",
        )?;
        assert_eq!(cmd.program, "wl-paste");
        assert_eq!(cmd.args, vec!["--no-newline", "--type", "text"]);
        Ok(())
    }

    #[test]
    fn read_x11_has_xsel_fallback() -> TestResult {
        let env = MapEnv::default().set("DISPLAY", ":0");
        let cmd = required(
            clipboard_read_command(ClipboardPlatform::Unix, &env),
            "X11 read command",
        )?;
        assert_eq!(cmd.program, "xclip");
        let (fb, args) = required(cmd.fallback, "X11 read fallback")?;
        assert_eq!(fb, "xsel");
        assert_eq!(args, vec!["--clipboard", "--output"]);
        Ok(())
    }

    #[test]
    fn extension_helper_matches_image_module() {
        assert_eq!(extension_for_image_mime_str("image/png"), Some("png"));
        assert_eq!(extension_for_image_mime_str("image/jpeg"), Some("jpg"));
    }

    #[test]
    fn select_preferred_prefers_png() {
        let types = "text/plain\nimage/jpeg\nimage/png\n";
        assert_eq!(
            select_preferred_image_mime(types).as_deref(),
            Some("image/png")
        );
    }

    #[tokio::test]
    async fn read_image_is_unavailable_on_non_unix() {
        let env = MapEnv::default();
        let cancel = CancellationToken::new();
        assert!(matches!(
            read_clipboard_image_with(ClipboardPlatform::Darwin, &env, &cancel).await,
            ClipboardReadResult::Unavailable
        ));
        assert!(matches!(
            read_clipboard_image_with(ClipboardPlatform::Windows, &env, &cancel).await,
            ClipboardReadResult::Unavailable
        ));
    }

    #[tokio::test]
    async fn read_image_is_empty_for_termux() {
        let env = MapEnv::default().set("TERMUX_VERSION", "1.0");
        let cancel = CancellationToken::new();
        assert!(matches!(
            read_clipboard_image_with(ClipboardPlatform::Unix, &env, &cancel).await,
            ClipboardReadResult::Empty
        ));
    }

    #[test]
    fn is_wsl_detects_env_vars() {
        assert!(is_wsl(&MapEnv::default().set("WSL_DISTRO_NAME", "Ubuntu")));
        assert!(is_wsl(&MapEnv::default().set("WSLENV", "x")));
        assert!(!is_wsl(&MapEnv::default()));
    }
}
