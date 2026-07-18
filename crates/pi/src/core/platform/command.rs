//! Typed platform command descriptions and process execution seams.

use std::io;
use std::process::{Command, Stdio};

/// Operating-system command family used by product integrations.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Platform {
    /// macOS.
    MacOs,
    /// Microsoft Windows.
    Windows,
    /// Linux and other Unix-like desktops.
    Unix,
}

impl Platform {
    /// Platform for the current build target.
    #[must_use]
    pub const fn current() -> Self {
        if cfg!(target_os = "macos") {
            Self::MacOs
        } else if cfg!(windows) {
            Self::Windows
        } else {
            Self::Unix
        }
    }
}

/// Fully separated executable and arguments. No shell parsing is implied.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CommandSpec {
    /// Executable name or path.
    pub program: String,
    /// Individual argv values, excluding argv zero.
    pub args: Vec<String>,
}

impl CommandSpec {
    /// Construct a command without invoking a shell.
    #[must_use]
    pub fn new(
        program: impl Into<String>,
        args: impl IntoIterator<Item = impl Into<String>>,
    ) -> Self {
        Self {
            program: program.into(),
            args: args.into_iter().map(Into::into).collect(),
        }
    }
}

/// Captured command completion.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CommandOutput {
    /// Exit status code, or `-1` when unavailable.
    pub status: i32,
    /// Captured stdout bytes.
    pub stdout: Vec<u8>,
    /// Captured stderr bytes.
    pub stderr: Vec<u8>,
}

impl CommandOutput {
    /// Whether the process exited successfully.
    #[must_use]
    pub fn success(&self) -> bool {
        self.status == 0
    }
}

/// Injectable process boundary used by platform services and packaging.
pub trait CommandRunner {
    /// Run a command with optional stdin and capture its output.
    ///
    /// # Errors
    ///
    /// Returns process creation, stdin, wait, or capture failures.
    fn run(&mut self, spec: &CommandSpec, stdin: Option<&[u8]>) -> io::Result<CommandOutput>;

    /// Spawn a detached command with all standard streams disconnected.
    ///
    /// # Errors
    ///
    /// Returns process creation failures.
    fn spawn_detached(&mut self, spec: &CommandSpec) -> io::Result<()>;
}

/// Real process runner.
#[derive(Default)]
pub struct SystemCommandRunner;

impl CommandRunner for SystemCommandRunner {
    fn run(&mut self, spec: &CommandSpec, stdin: Option<&[u8]>) -> io::Result<CommandOutput> {
        let mut command = Command::new(&spec.program);
        command
            .args(&spec.args)
            .stdin(if stdin.is_some() {
                Stdio::piped()
            } else {
                Stdio::null()
            })
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        let mut child = command.spawn()?;
        let writer = match (stdin, child.stdin.take()) {
            (Some(input), Some(mut child_stdin)) => {
                let owned = input.to_vec();
                Some(std::thread::spawn(move || {
                    use std::io::Write;
                    child_stdin.write_all(&owned)
                }))
            }
            _ => None,
        };
        let output = child.wait_with_output()?;
        if let Some(writer) = writer {
            writer.join().map_err(|_| {
                io::Error::other("clipboard stdin writer terminated unexpectedly")
            })??;
        }
        Ok(CommandOutput {
            status: output.status.code().unwrap_or(-1),
            stdout: output.stdout,
            stderr: output.stderr,
        })
    }

    fn spawn_detached(&mut self, spec: &CommandSpec) -> io::Result<()> {
        Command::new(&spec.program)
            .args(&spec.args)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .map(|_| ())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[cfg(unix)]
    type TestResult = Result<(), Box<dyn std::error::Error>>;

    #[test]
    fn command_spec_new_separates_program_and_args() {
        let spec = CommandSpec::new("pbcopy", Vec::<String>::new());
        assert_eq!(spec.program, "pbcopy");
        assert!(spec.args.is_empty());

        let spec = CommandSpec::new("xclip", ["-selection", "clipboard"]);
        assert_eq!(spec.program, "xclip");
        assert_eq!(
            spec.args,
            vec!["-selection".to_owned(), "clipboard".to_owned()]
        );
    }

    #[test]
    fn command_output_success_is_zero_exit() {
        let ok = CommandOutput {
            status: 0,
            stdout: Vec::new(),
            stderr: Vec::new(),
        };
        assert!(ok.success());

        let fail = CommandOutput {
            status: 3,
            stdout: Vec::new(),
            stderr: Vec::new(),
        };
        assert!(!fail.success());
    }

    #[test]
    fn platform_current_matches_build_target() {
        let current = Platform::current();
        #[cfg(target_os = "macos")]
        assert_eq!(current, Platform::MacOs);
        #[cfg(target_os = "windows")]
        assert_eq!(current, Platform::Windows);
        #[cfg(not(any(target_os = "macos", target_os = "windows")))]
        assert_eq!(current, Platform::Unix);
    }

    #[cfg(unix)]
    #[test]
    fn system_runner_runs_host_echo() -> TestResult {
        // Exercises the real `run` path with stdin piping and output capture
        // on a Unix host using `/bin/echo`, which is universally present.
        let mut runner = SystemCommandRunner;
        let spec = CommandSpec::new("/bin/echo", ["hello"]);
        let output = runner.run(&spec, None)?;
        assert!(output.success(), "status: {:?}", output.status);
        assert_eq!(String::from_utf8_lossy(&output.stdout).trim(), "hello");
        Ok(())
    }

    #[cfg(unix)]
    #[test]
    fn system_runner_pipes_stdin() -> TestResult {
        // `cat` reads stdin and echoes it back; proves the stdin-writer thread
        // feeds the child before wait_with_output captures stdout.
        let mut runner = SystemCommandRunner;
        let spec = CommandSpec::new("/bin/cat", Vec::<String>::new());
        let output = runner.run(&spec, Some(b"piped payload"))?;
        assert!(output.success());
        assert_eq!(output.stdout, b"piped payload");
        Ok(())
    }

    #[test]
    fn system_runner_reports_spawn_failure() {
        // A binary that almost certainly does not exist must surface an error
        // rather than panicking.
        let mut runner = SystemCommandRunner;
        let spec = CommandSpec::new("pi-definitely-not-a-real-binary-xyz", Vec::<String>::new());
        assert!(runner.run(&spec, None).is_err());
    }
}
