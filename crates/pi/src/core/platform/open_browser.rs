//! Cross-platform "open URL in browser" launcher.
//!
//! Ports `.references/pi/packages/coding-agent/src/utils/open-browser.ts`.
//!
//! This intentionally never invokes a shell. On Windows, `cmd /c start` is
//! avoided because `cmd.exe` re-parses metacharacters (`&`, `|`, `^`, ...)
//! before `start` runs, which would make attacker-controlled URLs injectable.
//! `rundll32 url.dll,FileProtocolHandler <url>` launches the registered
//! handler without a re-parse step. The argv is therefore the testable
//! cross-platform contract.

use std::process::{Command, Stdio};
use std::sync::OnceLock;

/// Platform discriminator used by [`open_browser_command`] to select the
/// launcher argv without consulting `std::env::consts` at call time, so unit
/// tests can exercise every branch on any host.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub enum BrowserPlatform {
    /// macOS: `open <target>`.
    Darwin,
    /// Windows: `rundll32 url.dll,FileProtocolHandler <target>`.
    Windows,
    /// Linux and other Unix: `xdg-open <target>`.
    Unix,
}

impl BrowserPlatform {
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

/// Resolved launcher argv `(command, args)` for `target` on `platform`.
///
/// The argv matches the TypeScript reference exactly so golden snapshots are
/// stable across host and target. Returned args include the target as the
/// final element.
#[must_use]
pub fn open_browser_command(platform: BrowserPlatform, target: &str) -> (String, Vec<String>) {
    match platform {
        BrowserPlatform::Darwin => ("open".to_owned(), vec![target.to_owned()]),
        BrowserPlatform::Windows => (
            "rundll32".to_owned(),
            vec!["url.dll,FileProtocolHandler".to_owned(), target.to_owned()],
        ),
        BrowserPlatform::Unix => ("xdg-open".to_owned(), vec![target.to_owned()]),
    }
}

/// Capture launcher-spawn errors for tests without panicking in production.
///
/// In production this is the no-op [`DefaultSpawnSink`], which silently
/// swallows the spawn error exactly like the TypeScript `.on("error", () => {})`
/// handler. Tests install a [`RecordingSpawnSink`] to assert that a missing
/// launcher is observed rather than crashing the process.
pub trait SpawnSink: Send + Sync {
    /// Called when spawning the launcher fails.
    fn on_error(&self, command: &str, error: std::io::Error);
}

/// Production sink: silently drop spawn errors.
#[derive(Debug, Default)]
pub struct DefaultSpawnSink;

impl SpawnSink for DefaultSpawnSink {
    fn on_error(&self, _command: &str, _error: std::io::Error) {}
}

/// Test sink: record the first spawn error.
#[derive(Debug, Default)]
pub struct RecordingSpawnSink {
    /// The first `(command, message)` pair observed, if any.
    pub error: std::sync::Mutex<Option<(String, String)>>,
}

impl SpawnSink for RecordingSpawnSink {
    fn on_error(&self, command: &str, error: std::io::Error) {
        let mut guard = match self.error.lock() {
            Ok(guard) => guard,
            Err(poisoned) => poisoned.into_inner(),
        };
        if guard.is_none() {
            *guard = Some((command.to_owned(), error.to_string()));
        }
    }
}

static SPAWN_SINK: OnceLock<Box<dyn SpawnSink>> = OnceLock::new();

/// Install a spawn-error sink. Subsequent [`open_browser_with`] calls route
/// spawn failures to it. Intended for tests; production leaves the default
/// no-op sink in place.
pub fn install_spawn_sink(sink: Box<dyn SpawnSink>) {
    let _ = SPAWN_SINK.set(sink);
}

fn report_error(command: &str, error: std::io::Error) {
    let sink = SPAWN_SINK.get_or_init(|| Box::<DefaultSpawnSink>::default());
    sink.on_error(command, error);
}

/// Open `target` (a URL or file path) in the platform default handler.
///
/// Best-effort and detached: the launcher is spawned with inherited-nothing
/// stdio, then reaped asynchronously. Launch failures are reported to the
/// installed [`SpawnSink`] and never panic, matching the TypeScript
/// fire-and-forget contract — callers still present the target to the user.
pub fn open_browser(target: &str) {
    open_browser_with(BrowserPlatform::host(), target);
}

/// Open `target` using an explicit platform selector.
///
/// Split from [`open_browser`] so the argv branch is unit-testable on any
/// host without depending on `cfg(target_os)`.
pub fn open_browser_with(platform: BrowserPlatform, target: &str) {
    let (command, args) = open_browser_command(platform, target);
    let spawn_result = Command::new(&command)
        .args(&args)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn();
    match spawn_result {
        Ok(mut child) => {
            // Detach: the TypeScript reference calls `.unref()`. We do not
            // await the child; a best-effort non-blocking poll releases the
            // handle so the launcher runs independently.
            child.try_wait().ok();
        }
        Err(error) => report_error(&command, error),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn darwin_argv_is_open_target() {
        let (cmd, args) = open_browser_command(BrowserPlatform::Darwin, "https://pi.dev");
        assert_eq!(cmd, "open");
        assert_eq!(args, vec!["https://pi.dev"]);
    }

    #[test]
    fn windows_argv_is_rundll32_fileprotocohandler_target() {
        let (cmd, args) =
            open_browser_command(BrowserPlatform::Windows, "https://pi.dev/session/#abc");
        assert_eq!(cmd, "rundll32");
        assert_eq!(
            args,
            vec!["url.dll,FileProtocolHandler", "https://pi.dev/session/#abc"]
        );
    }

    #[test]
    fn unix_argv_is_xdg_open_target() {
        let (cmd, args) = open_browser_command(BrowserPlatform::Unix, "https://pi.dev");
        assert_eq!(cmd, "xdg-open");
        assert_eq!(args, vec!["https://pi.dev"]);
    }

    #[test]
    fn host_platform_matches_cfg() {
        #[cfg(target_os = "macos")]
        assert_eq!(BrowserPlatform::host(), BrowserPlatform::Darwin);
        #[cfg(target_os = "windows")]
        assert_eq!(BrowserPlatform::host(), BrowserPlatform::Windows);
        #[cfg(not(any(target_os = "macos", target_os = "windows")))]
        assert_eq!(BrowserPlatform::host(), BrowserPlatform::Unix);
    }
}
