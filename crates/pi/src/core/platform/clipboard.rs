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

use std::fs;
use std::io::Write;
use std::process::{Command, Stdio};
use std::time::Duration;

use base64::Engine;
use uuid::Uuid;

use super::image::{convert_to_png, detect_supported_image_mime, extension_for_image_mime};

/// Maximum base64 length for an OSC 52 copy. Larger payloads are skipped to
/// avoid desynchronizing terminal rendering.
pub const MAX_OSC52_ENCODED_LENGTH: usize = 100_000;

/// Shell-tool spawn timeout for the synchronous clipboard helpers.
pub const CLIPBOARD_TIMEOUT: Duration = Duration::from_secs(5);

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

/// Errors returned by clipboard copy.
#[derive(Debug, thiserror::Error)]
pub enum ClipboardError {
    /// Every copy path (native, shell tool, OSC 52) failed.
    #[error("Failed to copy to clipboard")]
    Failed,
}

/// A clipboard image and its MIME type.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ClipboardImage {
    /// Raw image bytes.
    pub bytes: Vec<u8>,
    /// Canonical MIME type.
    pub mime: String,
}

/// A resolved clipboard write argv (program + args) with an optional fallback.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct WriteCommand {
    /// Program name.
    pub program: String,
    /// Argv excluding the program name.
    pub args: Vec<String>,
    /// Optional secondary argv tried when the primary is missing.
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
/// The argv matches the TypeScript reference's selection order exactly:
/// - Darwin → `pbcopy`
/// - Windows → `clip`
/// - Unix → Termux (`termux-clipboard-set`) if `TERMUX_VERSION` set, else
///   Wayland (`wl-copy`) when `is_wayland_session` and `WAYLAND_DISPLAY`,
///   else X11 (`xclip -selection clipboard`, which the reference falls back
///   from to `xsel --clipboard --input`).
#[must_use]
pub fn clipboard_write_command(
    platform: ClipboardPlatform,
    env: &dyn ClipboardEnv,
) -> Option<WriteCommand> {
    match platform {
        ClipboardPlatform::Darwin => Some(WriteCommand::new("pbcopy", vec![])),
        ClipboardPlatform::Windows => Some(WriteCommand::new("clip", vec![])),
        ClipboardPlatform::Unix => {
            if env.get("TERMUX_VERSION").is_some() {
                return Some(WriteCommand::new("termux-clipboard-set", vec![]));
            }
            let has_wayland = env.get("WAYLAND_DISPLAY").is_some();
            let has_x11 = env.get("DISPLAY").is_some();
            if is_wayland_session(env) && has_wayland {
                Some(WriteCommand::new("wl-copy", vec![]))
            } else if has_x11 {
                let mut cmd = WriteCommand::new(
                    "xclip",
                    vec!["-selection".to_owned(), "clipboard".to_owned()],
                );
                cmd.fallback = Some((
                    "xsel".to_owned(),
                    vec!["--clipboard".to_owned(), "--input".to_owned()],
                ));
                Some(cmd)
            } else {
                None
            }
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

/// Copy `text` with an explicit platform/env and OSC 52 sink.
///
/// Tries the selected shell tool (and its fallback) first: emitting OSC 52
/// before a tool copy can make terminals write the native clipboard twice,
/// and large payloads can desynchronize rendering. The sink receives the
/// encoded sequence when the OSC 52 path triggers — callers pass the
/// terminal's stdout handle; tests inject a capturing closure so the
/// decision logic is exercised without side effects.
///
/// # Errors
///
/// Returns [`ClipboardError::Failed`] when neither the selected platform tool
/// (including its fallback) nor OSC 52 can accept the text.
pub fn copy_to_clipboard_with(
    text: &str,
    platform: ClipboardPlatform,
    env: &dyn ClipboardEnv,
    emit_osc52: &mut dyn FnMut(&str),
) -> Result<(), ClipboardError> {
    let mut copied = false;

    if let Some(cmd) = clipboard_write_command(platform, env)
        && run_write_command(&cmd, text)
    {
        copied = true;
    }

    if (is_remote_session(env) || !copied)
        && let Some(sequence) = osc52_encode(text)
    {
        emit_osc52(&sequence);
        copied = true;
    }

    if copied {
        Ok(())
    } else {
        Err(ClipboardError::Failed)
    }
}

fn run_write_command(cmd: &WriteCommand, text: &str) -> bool {
    if pipe_to(&cmd.program, &cmd.args, text) {
        return true;
    }
    if let Some((program, args)) = &cmd.fallback {
        return pipe_to(program, args, text);
    }
    false
}

/// Pipe `text` to `program args` stdin within [`CLIPBOARD_TIMEOUT`]. Returns
/// `false` on spawn failure, timeout, or nonzero exit.
fn pipe_to(program: &str, args: &[String], text: &str) -> bool {
    let Ok(mut child) = Command::new(program)
        .args(args)
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
    else {
        return false;
    };
    if let Some(mut stdin) = child.stdin.take()
        && stdin.write_all(text.as_bytes()).is_err()
    {
        // EPIPE on early exit (e.g. wl-copy) is non-fatal; stdin is dropped.
    }
    match wait_timeout::ChildExt::wait_timeout(&mut child, CLIPBOARD_TIMEOUT) {
        Ok(Some(status)) => status.success(),
        Ok(None) => {
            let _ = child.kill();
            let _ = child.wait();
            false
        }
        Err(_) => false,
    }
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
#[must_use]
pub fn clipboard_read_command(
    platform: ClipboardPlatform,
    env: &dyn ClipboardEnv,
) -> Option<ReadCommand> {
    match platform {
        ClipboardPlatform::Darwin => Some(ReadCommand::new("pbpaste", vec![])),
        ClipboardPlatform::Windows => Some(ReadCommand::new(
            "powershell",
            vec![
                "-NoProfile".to_owned(),
                "-Command".to_owned(),
                "Get-Clipboard".to_owned(),
            ],
        )),
        ClipboardPlatform::Unix => {
            if is_wayland_session(env) && env.get("WAYLAND_DISPLAY").is_some() {
                Some(ReadCommand::new(
                    "wl-paste",
                    vec!["--no-newline".to_owned()],
                ))
            } else if env.get("DISPLAY").is_some() {
                let mut cmd = ReadCommand::new(
                    "xclip",
                    vec![
                        "-selection".to_owned(),
                        "clipboard".to_owned(),
                        "-o".to_owned(),
                    ],
                );
                cmd.fallback = Some((
                    "xsel".to_owned(),
                    vec!["--clipboard".to_owned(), "--output".to_owned()],
                ));
                Some(cmd)
            } else {
                None
            }
        }
    }
}

/// Read plain text from the clipboard on the host.
#[must_use]
pub fn read_clipboard_text() -> Option<String> {
    read_clipboard_text_with(ClipboardPlatform::host(), &HostEnv)
}

/// Read plain text with an explicit platform/env.
pub fn read_clipboard_text_with(
    platform: ClipboardPlatform,
    env: &dyn ClipboardEnv,
) -> Option<String> {
    let cmd = clipboard_read_command(platform, env)?;
    if let Some(text) = capture(&cmd.program, &cmd.args)
        && !text.is_empty()
    {
        return Some(text);
    }
    if let Some((program, args)) = &cmd.fallback {
        return capture(program, args).filter(|t| !t.is_empty());
    }
    None
}

fn capture(program: &str, args: &[String]) -> Option<String> {
    let output = Command::new(program)
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    Some(String::from_utf8_lossy(&output.stdout).into_owned())
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

/// Read an image from the clipboard, converting unsupported formats to PNG.
///
/// The argv selection mirrors `readClipboardImage`: Wayland/WSL → `wl-paste`
/// then `xclip`; WSL also tries PowerShell (Windows clipboard); plain X11
/// falls back to `xclip` directly. Termux yields no image. Unsupported MIME is
/// converted via [`maybe_convert_to_png`].
#[must_use]
pub fn read_clipboard_image() -> Option<ClipboardImage> {
    read_clipboard_image_with(ClipboardPlatform::host(), &HostEnv)
}

/// Read a clipboard image with an explicit platform/env.
pub fn read_clipboard_image_with(
    platform: ClipboardPlatform,
    env: &dyn ClipboardEnv,
) -> Option<ClipboardImage> {
    if env.get("TERMUX_VERSION").is_some() {
        return None;
    }
    let raw = read_clipboard_image_raw(platform, env)?;
    let (bytes, mime) = maybe_convert_to_png(&raw.bytes, &raw.mime)?;
    Some(ClipboardImage { bytes, mime })
}

fn read_clipboard_image_raw(
    platform: ClipboardPlatform,
    env: &dyn ClipboardEnv,
) -> Option<ClipboardImage> {
    if !matches!(platform, ClipboardPlatform::Unix) {
        return None;
    }
    let wayland = is_wayland_session(env);
    let wsl = is_wsl(env);

    // Mirrors readClipboardImage from the TypeScript reference:
    //  - on Wayland or WSL, try wl-paste first, then xclip
    //  - on WSL, also fall back to PowerShell (Windows clipboard)
    //  - on plain X11 (or any non-Wayland Linux) fall back to xclip.
    // The reference tries nativeClipboard before xclip, but this port has no
    // native addon, so xclip is the plain-X11 fallback.
    let mut image = None;
    if wayland || wsl {
        image = wl_paste_image().or_else(xclip_image);
    }
    if image.is_none() && wsl {
        image = read_clipboard_image_via_powershell();
    }
    if image.is_none() && !wayland {
        image = xclip_image();
    }
    image
}

fn wl_paste_image() -> Option<ClipboardImage> {
    let list = Command::new("wl-paste")
        .arg("--list-types")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .output()
        .ok()?;
    if !list.status.success() {
        return None;
    }
    let selected = select_preferred_image_mime(&String::from_utf8_lossy(&list.stdout))?;
    let data = Command::new("wl-paste")
        .args(["--type", &selected, "--no-newline"])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .output()
        .ok()?;
    if !data.status.success() || data.stdout.is_empty() {
        return None;
    }
    Some(ClipboardImage {
        bytes: data.stdout,
        mime: base_mime(&selected),
    })
}

fn xclip_image() -> Option<ClipboardImage> {
    for mime in ["image/png", "image/jpeg", "image/webp", "image/gif"] {
        let data = Command::new("xclip")
            .args(["-selection", "clipboard", "-t", mime, "-o"])
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .output()
            .ok()?;
        if data.status.success() && !data.stdout.is_empty() {
            return Some(ClipboardImage {
                bytes: data.stdout,
                mime: mime.to_owned(),
            });
        }
    }
    None
}

/// On WSL, the Linux clipboard often does not receive image data copied in
/// Windows (e.g. Win+Shift+S). PowerShell can reach the Windows clipboard
/// directly, so save a PNG to a temporary file and read it back.
fn read_clipboard_image_via_powershell() -> Option<ClipboardImage> {
    let tmp_file = std::env::temp_dir().join(format!("pi-wsl-clip-{}.png", Uuid::new_v4()));

    let win_path = {
        let output = Command::new("wslpath")
            .args(["-w", tmp_file.to_str()?])
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .output()
            .ok()?;
        if !output.status.success() {
            return None;
        }
        String::from_utf8_lossy(&output.stdout).trim().to_owned()
    };

    if win_path.is_empty() {
        return None;
    }

    let quoted = win_path.replace('\'', "''");
    let script = format!(
        "Add-Type -AssemblyName System.Windows.Forms; \
         Add-Type -AssemblyName System.Drawing; \
         $path = '{quoted}'; \
         $img = [System.Windows.Forms.Clipboard]::GetImage(); \
         if ($img) {{ $img.Save($path, [System.Drawing.Imaging.ImageFormat]::Png); Write-Output 'ok' }} else {{ Write-Output 'empty' }}"
    );

    let result = Command::new("powershell.exe")
        .args(["-NoProfile", "-Command", &script])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .output();

    let image = result.ok().and_then(|output| {
        if !output.status.success() {
            return None;
        }
        let text = String::from_utf8_lossy(&output.stdout).trim().to_owned();
        if text != "ok" {
            return None;
        }
        let bytes = fs::read(&tmp_file).ok()?;
        if bytes.is_empty() {
            return None;
        }
        Some(ClipboardImage {
            bytes,
            mime: "image/png".to_owned(),
        })
    });

    let _ = fs::remove_file(&tmp_file);
    image
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

    #[test]
    fn osc52_fallback_emits_sequence_when_no_tool_applies() -> TestResult {
        let env = MapEnv::default();
        let mut emitted = Vec::new();
        let result = copy_to_clipboard_with("hi", ClipboardPlatform::Unix, &env, &mut |seq| {
            emitted.push(seq.to_owned());
        });
        assert!(result.is_ok());
        assert_eq!(
            emitted,
            vec![required(osc52_encode("hi"), "OSC 52 sequence")?]
        );
        Ok(())
    }

    #[test]
    fn osc52_emits_in_remote_session() {
        let env = MapEnv::default().set("SSH_CONNECTION", "1.2.3.4");
        let mut emitted = Vec::new();
        let result = copy_to_clipboard_with("hi", ClipboardPlatform::Unix, &env, &mut |seq| {
            emitted.push(seq.to_owned());
        });
        assert!(result.is_ok());
        assert_eq!(emitted.len(), 1);
    }

    #[test]
    fn oversize_without_tool_errors_without_emit() {
        let env = MapEnv::default();
        let mut emitted = 0;
        let result = copy_to_clipboard_with(
            &"a".repeat(MAX_OSC52_ENCODED_LENGTH * 3 / 4 + 1),
            ClipboardPlatform::Unix,
            &env,
            &mut |_| emitted += 1,
        );
        assert!(matches!(result, Err(ClipboardError::Failed)));
        assert_eq!(emitted, 0);
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
    fn read_wayland_is_wl_paste_no_newline() -> TestResult {
        let env = MapEnv::default()
            .set("WAYLAND_DISPLAY", "wayland-0")
            .set("XDG_SESSION_TYPE", "wayland");
        let cmd = required(
            clipboard_read_command(ClipboardPlatform::Unix, &env),
            "Wayland read command",
        )?;
        assert_eq!(cmd.program, "wl-paste");
        assert_eq!(cmd.args, vec!["--no-newline"]);
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

    #[test]
    fn read_image_returns_none_on_non_unix() {
        let env = MapEnv::default();
        assert!(read_clipboard_image_with(ClipboardPlatform::Darwin, &env).is_none());
        assert!(read_clipboard_image_with(ClipboardPlatform::Windows, &env).is_none());
    }

    #[test]
    fn read_image_returns_none_for_termux() {
        let env = MapEnv::default().set("TERMUX_VERSION", "1.0");
        assert!(read_clipboard_image_with(ClipboardPlatform::Unix, &env).is_none());
    }

    #[test]
    fn is_wsl_detects_env_vars() {
        assert!(is_wsl(&MapEnv::default().set("WSL_DISTRO_NAME", "Ubuntu")));
        assert!(is_wsl(&MapEnv::default().set("WSLENV", "x")));
        assert!(!is_wsl(&MapEnv::default()));
    }
}
