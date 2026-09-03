//! Process-tree termination shared by every native spawn site.
//!
//! One owner for the platform kill path so the three spawn sites (bash tool,
//! package manager, config-value command runner) cannot drift apart: Unix
//! kills the detached process group (`process_group(0)` at spawn, mirroring
//! Node `detached`), Windows resolves `taskkill.exe` under `%SystemRoot%`
//! `\System32` per upstream `7af2d27dc` (#6596) instead of a bare `PATH`
//! search, falling back to `PATH` only when `SystemRoot` is unset.

#[cfg(any(windows, test))]
use std::path::PathBuf;
#[cfg(windows)]
use std::process::Stdio;

/// Kill a process and its descendants by OS pid.
///
/// Unix: `SIGKILL` the process group (spawn sites detach with
/// `process_group(0)`, so pgid == child pid), falling back to the pid.
/// Windows: `taskkill /F /T /PID`, resolved under `%SystemRoot%\System32`.
pub fn kill_process_tree(pid: u32) {
    #[cfg(unix)]
    {
        use nix::sys::signal::{Signal, kill, killpg};
        use nix::unistd::Pid;

        let Ok(raw) = i32::try_from(pid) else {
            return;
        };
        let group = Pid::from_raw(raw);
        // Node: process.kill(-pid, SIGKILL) then fallback process.kill(pid).
        if killpg(group, Signal::SIGKILL).is_err() {
            let _ = kill(group, Signal::SIGKILL);
        }
    }

    #[cfg(windows)]
    {
        let _ = std::process::Command::new(taskkill_path())
            .args(["/F", "/T", "/PID", &pid.to_string()])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn();
    }

    #[cfg(not(any(unix, windows)))]
    {
        let _ = pid;
    }
}

/// Resolve `taskkill.exe` under `%SystemRoot%\System32`, falling back to a
/// bare `PATH` search only when `SystemRoot` is unset.
#[cfg(windows)]
fn taskkill_path() -> PathBuf {
    std::env::var_os("SystemRoot")
        .map(PathBuf::from)
        .map(|root| root.join("System32").join("taskkill.exe"))
        .unwrap_or_else(|| PathBuf::from("taskkill.exe"))
}

/// Unit-testable view of the Windows resolution: the joined path for a given
/// `SystemRoot` value, or the `PATH` fallback name when unset.
#[cfg(test)]
#[must_use]
pub fn taskkill_path_for(system_root: Option<&str>) -> PathBuf {
    system_root.map_or_else(
        || PathBuf::from("taskkill.exe"),
        |root| PathBuf::from(root).join("System32").join("taskkill.exe"),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn taskkill_prefers_system_root_over_path() {
        assert_eq!(
            taskkill_path_for(Some("C:\\Windows")),
            PathBuf::from("C:\\Windows/System32/taskkill.exe")
        );
    }

    #[test]
    fn taskkill_falls_back_to_path_without_system_root() {
        assert_eq!(taskkill_path_for(None), PathBuf::from("taskkill.exe"));
    }
}
