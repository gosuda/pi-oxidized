//! Self-update planning, command execution, and atomic binary replacement.

use std::{
    ffi::OsString,
    fs,
    path::{Path, PathBuf},
};

use thiserror::Error;

use super::version_check::{LatestPiRelease, is_newer_package_version};

/// Supported installation ownership modes.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum InstallMethod {
    /// Standalone native binary.
    Binary,
    /// Rust package installed by Cargo.
    Cargo,
    /// npm global installation.
    Npm,
    /// pnpm global installation.
    Pnpm,
    /// Yarn global installation.
    Yarn,
    /// Bun global installation.
    Bun,
    /// Installation source cannot be proven.
    Unknown,
}

/// Paths used to determine how the current executable was installed.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct InstallEvidence {
    /// Current executable path.
    pub command_path: PathBuf,
    /// Package/source directory containing the executable, when known.
    pub source_path: Option<PathBuf>,
    /// Explicit marker supplied by a packaged standalone binary.
    pub standalone_binary: bool,
}

/// Determine installation mode without invoking a package manager.
#[must_use]
pub fn detect_install_method(evidence: &InstallEvidence) -> InstallMethod {
    if evidence.standalone_binary {
        return InstallMethod::Binary;
    }
    let mut joined = evidence.command_path.to_string_lossy().to_lowercase();
    if let Some(source) = &evidence.source_path {
        joined.push('\0');
        joined.push_str(&source.to_string_lossy().to_lowercase());
    }
    let normalized = joined.replace('\\', "/");
    if normalized.contains("/.cargo/bin/") || normalized.ends_with("/.cargo/bin/pi") {
        InstallMethod::Cargo
    } else if normalized.contains("/.pnpm/") || normalized.contains("/pnpm/") {
        InstallMethod::Pnpm
    } else if normalized.contains("/.yarn/") || normalized.contains("/yarn/") {
        InstallMethod::Yarn
    } else if normalized.contains("/.bun/") || normalized.contains("/bun/") {
        InstallMethod::Bun
    } else if normalized.contains("/node_modules/") || normalized.contains("/npm/") {
        InstallMethod::Npm
    } else {
        InstallMethod::Unknown
    }
}

/// One subprocess in a self-update operation.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CommandStep {
    /// Program name or absolute path.
    pub program: OsString,
    /// Exact argument vector.
    pub args: Vec<OsString>,
}

impl CommandStep {
    fn new(
        program: impl Into<OsString>,
        args: impl IntoIterator<Item = impl Into<OsString>>,
    ) -> Self {
        Self {
            program: program.into(),
            args: args.into_iter().map(Into::into).collect(),
        }
    }
}

/// How an update will be applied.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum UpdateAction {
    /// Execute package-manager commands in order.
    Commands(Vec<CommandStep>),
    /// Replace the current executable with an already-downloaded file.
    ReplaceBinary {
        /// Running executable.
        current: PathBuf,
        /// Fully downloaded and verified replacement.
        replacement: PathBuf,
        /// Rollback file retained until replacement succeeds.
        backup: PathBuf,
    },
}

/// Complete, side-effect-free update plan.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SelfUpdatePlan {
    /// Installed package name.
    pub installed_package_name: String,
    /// Target package name.
    pub package_name: String,
    /// `<package>@<version>` install spec.
    pub install_spec: String,
    /// Target version.
    pub version: String,
    /// Optional release note.
    pub note: Option<String>,
    /// Whether force/version/package-rename rules require execution.
    pub should_run: bool,
    /// Installation action, absent when no update should run.
    pub action: Option<UpdateAction>,
}

/// Flags affecting update execution.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct UpdateOptions {
    /// Reinstall even when the version is not newer.
    pub force: bool,
    /// Return the plan without changing files or spawning commands.
    pub dry_run: bool,
    /// Prohibit endpoint access and update execution.
    pub offline: bool,
}

/// Self-update failure.
#[derive(Debug, Error)]
pub enum UpdateError {
    /// Offline mode prohibits updating.
    #[error("self-update is unavailable while offline")]
    Offline,
    /// Installation ownership cannot be established.
    #[error("this installation is not managed by a supported update method")]
    UnsupportedInstallation,
    /// Child process failed.
    #[error("update command failed: {0}")]
    Command(String),
    /// File operation failed.
    #[error("update file operation failed: {0}")]
    Io(#[from] std::io::Error),
    /// Replacement failed and rollback also failed.
    #[error("replacement failed ({replace}); rollback failed ({rollback})")]
    Rollback {
        /// Replacement error.
        replace: std::io::Error,
        /// Rollback error.
        rollback: std::io::Error,
    },
}

/// Build exact package-manager argv for an installation mode.
#[must_use]
pub fn get_self_update_command(
    method: InstallMethod,
    installed_package_name: &str,
    package_name: &str,
    install_spec: &str,
    npm_command: Option<&[OsString]>,
    pnpm_global_bin_dir: Option<&Path>,
) -> Option<Vec<CommandStep>> {
    let renamed = installed_package_name != package_name;
    let mut steps = Vec::with_capacity(2);
    match method {
        InstallMethod::Npm => {
            let (program, prefix) = npm_command
                .and_then(|command| command.split_first())
                .map_or_else(
                    || (OsString::from("npm"), Vec::new()),
                    |(program, args)| (program.clone(), args.to_vec()),
                );
            if renamed {
                let mut args = prefix.clone();
                args.extend([OsString::from("uninstall"), OsString::from("-g")]);
                args.push(OsString::from(installed_package_name));
                steps.push(CommandStep {
                    program: program.clone(),
                    args,
                });
            }
            let mut args = prefix;
            args.extend([
                OsString::from("install"),
                OsString::from("-g"),
                OsString::from("--ignore-scripts"),
                OsString::from("--min-release-age=0"),
                OsString::from(install_spec),
            ]);
            steps.push(CommandStep { program, args });
        }
        InstallMethod::Pnpm => {
            let bin_arg = pnpm_global_bin_dir
                .map(|path| OsString::from(format!("--config.global-bin-dir={}", path.display())));
            if renamed {
                let mut args = vec![OsString::from("remove"), OsString::from("-g")];
                if let Some(arg) = &bin_arg {
                    args.push(arg.clone());
                }
                args.push(OsString::from(installed_package_name));
                steps.push(CommandStep::new("pnpm", args));
            }
            let mut args = vec![
                OsString::from("install"),
                OsString::from("-g"),
                OsString::from("--ignore-scripts"),
                OsString::from("--config.minimumReleaseAge=0"),
            ];
            if let Some(arg) = bin_arg {
                args.push(arg);
            }
            args.push(OsString::from(install_spec));
            steps.push(CommandStep::new("pnpm", args));
        }
        InstallMethod::Yarn => {
            if renamed {
                steps.push(CommandStep::new(
                    "yarn",
                    ["global", "remove", installed_package_name],
                ));
            }
            steps.push(CommandStep::new(
                "yarn",
                ["global", "add", "--ignore-scripts", install_spec],
            ));
        }
        InstallMethod::Bun => {
            if renamed {
                steps.push(CommandStep::new(
                    "bun",
                    ["uninstall", "-g", installed_package_name],
                ));
            }
            steps.push(CommandStep::new(
                "bun",
                [
                    "install",
                    "-g",
                    "--ignore-scripts",
                    "--minimum-release-age=0",
                    install_spec,
                ],
            ));
        }
        InstallMethod::Cargo => steps.push(CommandStep::new(
            "cargo",
            [
                "install",
                package_name,
                "--version",
                install_spec
                    .rsplit_once('@')
                    .map_or(install_spec, |(_, version)| version),
                "--locked",
                "--force",
            ],
        )),
        InstallMethod::Binary | InstallMethod::Unknown => return None,
    }
    Some(steps)
}

/// Resolve release metadata and install mode into a pure plan.
///
/// # Errors
///
/// Returns [`UpdateError::Offline`] when `options.offline` is set and
/// [`UpdateError::UnsupportedInstallation`] when the install method has no
/// package-manager command.
pub fn build_self_update_plan(
    current_version: &str,
    installed_package_name: &str,
    release: LatestPiRelease,
    method: InstallMethod,
    options: UpdateOptions,
    npm_command: Option<&[OsString]>,
    pnpm_global_bin_dir: Option<&Path>,
) -> Result<SelfUpdatePlan, UpdateError> {
    if options.offline {
        return Err(UpdateError::Offline);
    }
    let package_name = release
        .package_name
        .clone()
        .unwrap_or_else(|| installed_package_name.to_owned());
    let install_spec = format!("{package_name}@{}", release.version);
    let should_run = options.force
        || package_name != installed_package_name
        || is_newer_package_version(&release.version, current_version);
    let action = if should_run {
        let commands = get_self_update_command(
            method,
            installed_package_name,
            &package_name,
            &install_spec,
            npm_command,
            pnpm_global_bin_dir,
        )
        .ok_or(UpdateError::UnsupportedInstallation)?;
        Some(UpdateAction::Commands(commands))
    } else {
        None
    };
    Ok(SelfUpdatePlan {
        installed_package_name: installed_package_name.to_owned(),
        package_name,
        install_spec,
        version: release.version,
        note: release.note,
        should_run,
        action,
    })
}

/// Build a standalone-binary replacement plan after the artifact has been
/// downloaded and verified by the caller.
///
/// # Errors
///
/// Returns [`UpdateError::Offline`] when `options.offline` is set.
pub fn build_binary_self_update_plan(
    current_version: &str,
    installed_package_name: &str,
    release: LatestPiRelease,
    options: UpdateOptions,
    current: PathBuf,
    replacement: PathBuf,
    backup: PathBuf,
) -> Result<SelfUpdatePlan, UpdateError> {
    if options.offline {
        return Err(UpdateError::Offline);
    }
    let package_name = release
        .package_name
        .clone()
        .unwrap_or_else(|| installed_package_name.to_owned());
    let install_spec = format!("{package_name}@{}", release.version);
    let should_run = options.force
        || package_name != installed_package_name
        || is_newer_package_version(&release.version, current_version);
    Ok(SelfUpdatePlan {
        installed_package_name: installed_package_name.to_owned(),
        package_name,
        install_spec,
        version: release.version,
        note: release.note,
        should_run,
        action: should_run.then_some(UpdateAction::ReplaceBinary {
            current,
            replacement,
            backup,
        }),
    })
}

/// Injected command runner.
pub trait UpdateRunner {
    /// Run one step and wait for completion.
    ///
    /// # Errors
    ///
    /// Returns [`UpdateError::Command`] when the subprocess exits non-zero, and
    /// [`UpdateError::Io`] for spawning or I/O failure.
    fn run(&mut self, step: &CommandStep) -> Result<(), UpdateError>;
}

/// Standard inherited-stdio command runner.
pub struct ProcessUpdateRunner;

impl UpdateRunner for ProcessUpdateRunner {
    fn run(&mut self, step: &CommandStep) -> Result<(), UpdateError> {
        let status = std::process::Command::new(&step.program)
            .args(&step.args)
            .status()?;
        if status.success() {
            Ok(())
        } else {
            Err(UpdateError::Command(format!(
                "{} exited with {}",
                Path::new(&step.program).display(),
                status
            )))
        }
    }
}

/// Execute a plan with the real filesystem. Dry-run and no-op plans have no effects.
///
/// # Errors
///
/// Propagates [`UpdateError::Offline`], [`UpdateError::Command`],
/// [`UpdateError::Io`], and [`UpdateError::Rollback`] from plan execution.
pub fn run_self_update(
    plan: &SelfUpdatePlan,
    options: UpdateOptions,
    runner: &mut dyn UpdateRunner,
) -> Result<(), UpdateError> {
    run_self_update_with_filesystem(plan, options, runner, &StdUpdateFileSystem)
}

/// Fully injected self-update executor.
///
/// # Errors
///
/// See [`run_self_update`].
pub fn run_self_update_with_filesystem(
    plan: &SelfUpdatePlan,
    options: UpdateOptions,
    runner: &mut dyn UpdateRunner,
    filesystem: &dyn UpdateFileSystem,
) -> Result<(), UpdateError> {
    if options.offline {
        return Err(UpdateError::Offline);
    }
    if options.dry_run || !plan.should_run {
        return Ok(());
    }
    match &plan.action {
        Some(UpdateAction::Commands(steps)) => {
            for step in steps {
                runner.run(step)?;
            }
            Ok(())
        }
        Some(UpdateAction::ReplaceBinary {
            current,
            replacement,
            backup,
        }) => atomic_replace_binary(filesystem, current, replacement, backup),
        None => Ok(()),
    }
}

/// Filesystem seam used by atomic replacement and Windows quarantine.
pub trait UpdateFileSystem {
    /// Whether a path exists.
    fn exists(&self, path: &Path) -> bool;
    /// Rename a path atomically within a filesystem.
    ///
    /// # Errors
    ///
    /// Propagates the underlying [`std::io`] error when the rename fails.
    fn rename(&self, from: &Path, to: &Path) -> std::io::Result<()>;
    /// Copy a file.
    ///
    /// # Errors
    ///
    /// Propagates the underlying [`std::io`] error when the copy fails.
    fn copy(&self, from: &Path, to: &Path) -> std::io::Result<u64>;
    /// Remove a file if present.
    ///
    /// # Errors
    ///
    /// Propagates the underlying [`std::io`] error when the removal fails.
    fn remove_file(&self, path: &Path) -> std::io::Result<()>;
    /// Recursively remove a directory if present.
    ///
    /// # Errors
    ///
    /// Propagates the underlying [`std::io`] error when the removal fails.
    fn remove_dir_all(&self, path: &Path) -> std::io::Result<()>;
    /// Recursively create a directory.
    ///
    /// # Errors
    ///
    /// Propagates the underlying [`std::io`] error when creation fails.
    fn create_dir_all(&self, path: &Path) -> std::io::Result<()>;
    /// Read file permissions.
    ///
    /// # Errors
    ///
    /// Propagates the underlying [`std::io`] error when metadata read fails.
    fn permissions(&self, path: &Path) -> std::io::Result<fs::Permissions>;
    /// Set file permissions.
    ///
    /// # Errors
    ///
    /// Propagates the underlying [`std::io`] error when setting permissions fails.
    fn set_permissions(&self, path: &Path, permissions: fs::Permissions) -> std::io::Result<()>;
}

/// Real filesystem implementation.
pub struct StdUpdateFileSystem;

impl UpdateFileSystem for StdUpdateFileSystem {
    fn exists(&self, path: &Path) -> bool {
        path.exists()
    }
    fn rename(&self, from: &Path, to: &Path) -> std::io::Result<()> {
        fs::rename(from, to)
    }
    fn copy(&self, from: &Path, to: &Path) -> std::io::Result<u64> {
        fs::copy(from, to)
    }
    fn remove_file(&self, path: &Path) -> std::io::Result<()> {
        fs::remove_file(path)
    }
    fn remove_dir_all(&self, path: &Path) -> std::io::Result<()> {
        fs::remove_dir_all(path)
    }
    fn create_dir_all(&self, path: &Path) -> std::io::Result<()> {
        fs::create_dir_all(path)
    }
    fn permissions(&self, path: &Path) -> std::io::Result<fs::Permissions> {
        fs::metadata(path).map(|m| m.permissions())
    }
    fn set_permissions(&self, path: &Path, permissions: fs::Permissions) -> std::io::Result<()> {
        fs::set_permissions(path, permissions)
    }
}

/// Atomically replace an executable and restore the old file on failure.
///
/// Once the replacement is renamed into position the operation is considered
/// successful; backup cleanup failure is swallowed because the update has
/// already taken effect.
///
/// # Errors
///
/// Returns [`UpdateError::Io`] for permission or rename failure;
/// [`UpdateError::Rollback`] when both replacement and rollback fail.
pub fn atomic_replace_binary(
    filesystem: &dyn UpdateFileSystem,
    current: &Path,
    replacement: &Path,
    backup: &Path,
) -> Result<(), UpdateError> {
    let permissions = filesystem.permissions(current)?;
    filesystem.set_permissions(replacement, permissions)?;
    if filesystem.exists(backup) {
        filesystem.remove_file(backup)?;
    }
    filesystem.rename(current, backup)?;
    if let Err(replace) = filesystem.rename(replacement, current) {
        return match filesystem.rename(backup, current) {
            Ok(()) => Err(UpdateError::Io(replace)),
            Err(rollback) => Err(UpdateError::Rollback { replace, rollback }),
        };
    }
    // Best-effort backup cleanup; the replacement already succeeded.
    let _ = filesystem.remove_file(backup);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn strings(step: &CommandStep) -> Vec<String> {
        step.args
            .iter()
            .map(|value| value.to_string_lossy().into_owned())
            .collect()
    }

    #[test]
    fn detection_uses_command_and_source_paths() {
        let evidence = InstallEvidence {
            command_path: PathBuf::from("/home/me/.cargo/bin/pi"),
            source_path: None,
            standalone_binary: false,
        };
        assert_eq!(detect_install_method(&evidence), InstallMethod::Cargo);
        let evidence = InstallEvidence {
            command_path: PathBuf::from("/usr/bin/node"),
            source_path: Some(PathBuf::from("/opt/pnpm/global/5/node_modules/pkg")),
            standalone_binary: false,
        };
        assert_eq!(detect_install_method(&evidence), InstallMethod::Pnpm);
    }

    #[test]
    fn detect_install_method_covers_all_paths_and_windows_backslashes() {
        // Standalone binary flag always wins.
        let evidence = InstallEvidence {
            command_path: PathBuf::from("/home/me/pi"),
            source_path: None,
            standalone_binary: true,
        };
        assert_eq!(detect_install_method(&evidence), InstallMethod::Binary);

        // Cargo via command_path.
        let evidence = InstallEvidence {
            command_path: PathBuf::from("/home/me/.cargo/bin/pi"),
            ..InstallEvidence::default()
        };
        assert_eq!(detect_install_method(&evidence), InstallMethod::Cargo);

        // Ends-with cargo bin pi.
        let evidence = InstallEvidence {
            command_path: PathBuf::from("/usr/local/.cargo/bin/pi"),
            ..InstallEvidence::default()
        };
        assert_eq!(detect_install_method(&evidence), InstallMethod::Cargo);

        // pnpm via .pnpm.
        let evidence = InstallEvidence {
            command_path: PathBuf::from("/opt/.pnpm/global/5/node_modules/pi"),
            ..InstallEvidence::default()
        };
        assert_eq!(detect_install_method(&evidence), InstallMethod::Pnpm);

        // yarn.
        let evidence = InstallEvidence {
            command_path: PathBuf::from("/home/.yarn/global/pi"),
            ..InstallEvidence::default()
        };
        assert_eq!(detect_install_method(&evidence), InstallMethod::Yarn);

        // bun.
        let evidence = InstallEvidence {
            command_path: PathBuf::from("/home/.bun/install/global/pi"),
            ..InstallEvidence::default()
        };
        assert_eq!(detect_install_method(&evidence), InstallMethod::Bun);

        // npm via node_modules.
        let evidence = InstallEvidence {
            command_path: PathBuf::from("/usr/lib/node_modules/pi/bin/pi"),
            ..InstallEvidence::default()
        };
        assert_eq!(detect_install_method(&evidence), InstallMethod::Npm);

        // Unknown.
        let evidence = InstallEvidence {
            command_path: PathBuf::from("/usr/local/bin/pi"),
            ..InstallEvidence::default()
        };
        assert_eq!(detect_install_method(&evidence), InstallMethod::Unknown);

        // Windows backslash paths are normalized to forward slashes.
        let evidence = InstallEvidence {
            command_path: PathBuf::from(r"C:\Users\me\.cargo\bin\pi.exe"),
            ..InstallEvidence::default()
        };
        assert_eq!(detect_install_method(&evidence), InstallMethod::Cargo);

        // Source path contributes to detection.
        let evidence = InstallEvidence {
            command_path: PathBuf::from("/usr/bin/node"),
            source_path: Some(PathBuf::from("/home/.yarn/global/pi")),
            standalone_binary: false,
        };
        assert_eq!(detect_install_method(&evidence), InstallMethod::Yarn);
    }

    #[test]
    fn package_manager_argv_are_exact() {
        let npm =
            get_self_update_command(InstallMethod::Npm, "old", "new", "new@2.0.0", None, None);
        assert!(npm.is_some());
        let npm = npm.unwrap_or_default();
        assert_eq!(strings(&npm[0]), ["uninstall", "-g", "old"]);
        assert_eq!(
            strings(&npm[1]),
            [
                "install",
                "-g",
                "--ignore-scripts",
                "--min-release-age=0",
                "new@2.0.0"
            ]
        );

        let pnpm = get_self_update_command(
            InstallMethod::Pnpm,
            "pi",
            "pi",
            "pi@2.0.0",
            None,
            Some(Path::new("/global/bin")),
        )
        .unwrap_or_default();
        assert_eq!(
            strings(&pnpm[0]),
            [
                "install",
                "-g",
                "--ignore-scripts",
                "--config.minimumReleaseAge=0",
                "--config.global-bin-dir=/global/bin",
                "pi@2.0.0"
            ]
        );
        let bun = get_self_update_command(InstallMethod::Bun, "pi", "pi", "pi@2.0.0", None, None)
            .unwrap_or_default();
        assert_eq!(
            strings(&bun[0]),
            [
                "install",
                "-g",
                "--ignore-scripts",
                "--minimum-release-age=0",
                "pi@2.0.0"
            ]
        );
        let cargo =
            get_self_update_command(InstallMethod::Cargo, "pi", "pi", "pi@2.0.0", None, None)
                .unwrap_or_default();
        assert_eq!(
            strings(&cargo[0]),
            ["install", "pi", "--version", "2.0.0", "--locked", "--force"]
        );
    }

    #[test]
    fn npm_argv_uses_custom_npm_command_prefix() {
        let prefix = [
            OsString::from("mise"),
            OsString::from("exec"),
            OsString::from("node@20"),
            OsString::from("--"),
            OsString::from("npm"),
        ];
        let npm = get_self_update_command(
            InstallMethod::Npm,
            "pi",
            "pi",
            "pi@2.0.0",
            Some(&prefix),
            None,
        );
        assert!(npm.is_some());
        let npm = npm.unwrap_or_default();
        // Single install step (no rename).
        assert_eq!(npm.len(), 1);
        assert_eq!(npm[0].program, OsString::from("mise"));
        assert_eq!(
            strings(&npm[0]),
            [
                "exec",
                "node@20",
                "--",
                "npm",
                "install",
                "-g",
                "--ignore-scripts",
                "--min-release-age=0",
                "pi@2.0.0"
            ]
        );
    }

    #[test]
    fn npm_argv_uninstall_precedes_install_on_rename() {
        let npm = get_self_update_command(
            InstallMethod::Npm,
            "old-pkg",
            "new-pkg",
            "new-pkg@3.0.0",
            None,
            None,
        );
        assert!(npm.is_some());
        let npm = npm.unwrap_or_default();
        assert_eq!(npm.len(), 2);
        // Uninstall first, then install.
        assert_eq!(strings(&npm[0]), ["uninstall", "-g", "old-pkg"]);
        assert_eq!(
            strings(&npm[1]),
            [
                "install",
                "-g",
                "--ignore-scripts",
                "--min-release-age=0",
                "new-pkg@3.0.0"
            ]
        );
    }

    #[test]
    fn pnpm_argv_uninstall_includes_bin_dir_on_rename() {
        let pnpm = get_self_update_command(
            InstallMethod::Pnpm,
            "old",
            "new",
            "new@2.0.0",
            None,
            Some(Path::new("/pnpm/global/bin")),
        );
        assert!(pnpm.is_some());
        let pnpm = pnpm.unwrap_or_default();
        assert_eq!(pnpm.len(), 2);
        assert_eq!(
            strings(&pnpm[0]),
            [
                "remove",
                "-g",
                "--config.global-bin-dir=/pnpm/global/bin",
                "old"
            ]
        );
        assert_eq!(
            strings(&pnpm[1]),
            [
                "install",
                "-g",
                "--ignore-scripts",
                "--config.minimumReleaseAge=0",
                "--config.global-bin-dir=/pnpm/global/bin",
                "new@2.0.0"
            ]
        );
    }

    #[test]
    fn yarn_argv_matches_typescript_shape() {
        // No rename: single install step.
        let yarn = get_self_update_command(InstallMethod::Yarn, "pi", "pi", "pi@2.0.0", None, None);
        assert!(yarn.is_some());
        let yarn = yarn.unwrap_or_default();
        assert_eq!(yarn.len(), 1);
        assert_eq!(
            strings(&yarn[0]),
            ["global", "add", "--ignore-scripts", "pi@2.0.0"]
        );

        // Rename: uninstall + install.
        let yarn =
            get_self_update_command(InstallMethod::Yarn, "old", "new", "new@2.0.0", None, None);
        assert!(yarn.is_some());
        let yarn = yarn.unwrap_or_default();
        assert_eq!(yarn.len(), 2);
        assert_eq!(strings(&yarn[0]), ["global", "remove", "old"]);
        assert_eq!(
            strings(&yarn[1]),
            ["global", "add", "--ignore-scripts", "new@2.0.0"]
        );
    }

    #[test]
    fn bun_argv_uninstall_precedes_install_on_rename() {
        let bun =
            get_self_update_command(InstallMethod::Bun, "old", "new", "new@2.0.0", None, None);
        assert!(bun.is_some());
        let bun = bun.unwrap_or_default();
        assert_eq!(bun.len(), 2);
        assert_eq!(strings(&bun[0]), ["uninstall", "-g", "old"]);
        assert_eq!(
            strings(&bun[1]),
            [
                "install",
                "-g",
                "--ignore-scripts",
                "--minimum-release-age=0",
                "new@2.0.0"
            ]
        );
    }

    #[test]
    fn binary_and_unknown_methods_return_none() {
        assert!(
            get_self_update_command(InstallMethod::Binary, "pi", "pi", "pi@2.0.0", None, None)
                .is_none()
        );
        assert!(
            get_self_update_command(InstallMethod::Unknown, "pi", "pi", "pi@2.0.0", None, None)
                .is_none()
        );
    }

    #[test]
    fn cargo_argv_strips_version_from_install_spec() {
        // Standard spec: pi@2.0.0 -> version 2.0.0.
        let cargo =
            get_self_update_command(InstallMethod::Cargo, "pi", "pi", "pi@2.0.0", None, None);
        assert!(cargo.is_some());
        let cargo = cargo.unwrap_or_default();
        assert_eq!(
            strings(&cargo[0]),
            ["install", "pi", "--version", "2.0.0", "--locked", "--force"]
        );

        // Renamed package: cargo uses the target package_name, not installed name.
        let cargo = get_self_update_command(
            InstallMethod::Cargo,
            "old-name",
            "new-name",
            "new-name@3.0.0",
            None,
            None,
        );
        assert!(cargo.is_some());
        let cargo = cargo.unwrap_or_default();
        assert_eq!(
            strings(&cargo[0]),
            [
                "install",
                "new-name",
                "--version",
                "3.0.0",
                "--locked",
                "--force"
            ]
        );
    }

    #[derive(Default)]
    struct RecordingRunner {
        calls: Vec<CommandStep>,
    }
    impl UpdateRunner for RecordingRunner {
        fn run(&mut self, step: &CommandStep) -> Result<(), UpdateError> {
            self.calls.push(step.clone());
            Ok(())
        }
    }

    #[derive(Default)]
    struct FailingRunner {
        error: Option<UpdateError>,
    }
    impl UpdateRunner for FailingRunner {
        fn run(&mut self, _step: &CommandStep) -> Result<(), UpdateError> {
            Err(self
                .error
                .take()
                .unwrap_or(UpdateError::Command("no error configured".to_owned())))
        }
    }

    #[test]
    fn force_dry_run_and_idempotency_are_explicit() -> Result<(), UpdateError> {
        let release = LatestPiRelease {
            version: "1.0.0".to_owned(),
            package_name: None,
            note: None,
        };
        let plan = build_self_update_plan(
            "1.0.0",
            "pi",
            release.clone(),
            InstallMethod::Npm,
            UpdateOptions::default(),
            None,
            None,
        )?;
        assert!(!plan.should_run);
        let forced = build_self_update_plan(
            "1.0.0",
            "pi",
            release,
            InstallMethod::Npm,
            UpdateOptions {
                force: true,
                ..UpdateOptions::default()
            },
            None,
            None,
        )?;
        let mut runner = RecordingRunner::default();
        run_self_update(
            &forced,
            UpdateOptions {
                dry_run: true,
                ..UpdateOptions::default()
            },
            &mut runner,
        )?;
        assert!(runner.calls.is_empty());
        run_self_update(&forced, UpdateOptions::default(), &mut runner)?;
        assert_eq!(runner.calls.len(), 1);
        Ok(())
    }

    #[test]
    fn build_plan_offline_returns_error() {
        let release = LatestPiRelease {
            version: "2.0.0".to_owned(),
            package_name: None,
            note: None,
        };
        let result = build_self_update_plan(
            "1.0.0",
            "pi",
            release,
            InstallMethod::Npm,
            UpdateOptions {
                offline: true,
                ..UpdateOptions::default()
            },
            None,
            None,
        );
        assert!(matches!(result, Err(UpdateError::Offline)));
    }

    #[test]
    fn build_plan_unsupported_install_returns_error_when_update_needed() {
        let release = LatestPiRelease {
            version: "2.0.0".to_owned(),
            package_name: None,
            note: None,
        };
        let result = build_self_update_plan(
            "1.0.0",
            "pi",
            release,
            InstallMethod::Binary,
            UpdateOptions::default(),
            None,
            None,
        );
        assert!(matches!(result, Err(UpdateError::UnsupportedInstallation)));
    }

    #[test]
    fn build_plan_newer_version_triggers_run() -> Result<(), UpdateError> {
        let release = LatestPiRelease {
            version: "2.0.0".to_owned(),
            package_name: None,
            note: Some("major".to_owned()),
        };
        let plan = build_self_update_plan(
            "1.0.0",
            "pi",
            release,
            InstallMethod::Npm,
            UpdateOptions::default(),
            None,
            None,
        )?;
        assert!(plan.should_run);
        assert_eq!(plan.version, "2.0.0");
        assert_eq!(plan.note, Some("major".to_owned()));
        assert_eq!(plan.install_spec, "pi@2.0.0");
        assert!(plan.action.is_some());
        Ok(())
    }

    #[test]
    fn build_plan_renamed_package_triggers_run_even_on_same_version() -> Result<(), UpdateError> {
        let release = LatestPiRelease {
            version: "1.0.0".to_owned(),
            package_name: Some("pi-new".to_owned()),
            note: None,
        };
        let plan = build_self_update_plan(
            "1.0.0",
            "pi-old",
            release,
            InstallMethod::Npm,
            UpdateOptions::default(),
            None,
            None,
        )?;
        assert!(plan.should_run);
        assert_eq!(plan.package_name, "pi-new");
        assert_eq!(plan.install_spec, "pi-new@1.0.0");
        // Rename produces two steps: uninstall old, install new.
        assert!(
            matches!(&plan.action, Some(UpdateAction::Commands(steps)) if steps.len() == 2),
            "expected Commands action with 2 steps for renamed package"
        );
        Ok(())
    }

    #[test]
    fn build_binary_plan_offline_returns_error() {
        let release = LatestPiRelease {
            version: "2.0.0".to_owned(),
            package_name: None,
            note: None,
        };
        let result = build_binary_self_update_plan(
            "1.0.0",
            "pi",
            release,
            UpdateOptions {
                offline: true,
                ..UpdateOptions::default()
            },
            PathBuf::from("/cur"),
            PathBuf::from("/new"),
            PathBuf::from("/bak"),
        );
        assert!(matches!(result, Err(UpdateError::Offline)));
    }

    #[test]
    fn build_binary_plan_replace_action_when_newer() -> Result<(), UpdateError> {
        let release = LatestPiRelease {
            version: "2.0.0".to_owned(),
            package_name: None,
            note: None,
        };
        let plan = build_binary_self_update_plan(
            "1.0.0",
            "pi",
            release,
            UpdateOptions::default(),
            PathBuf::from("/cur/pi"),
            PathBuf::from("/tmp/new-pi"),
            PathBuf::from("/cur/pi.bak"),
        )?;
        assert!(plan.should_run);
        assert!(
            matches!(
                plan.action.as_ref(),
                Some(UpdateAction::ReplaceBinary { .. })
            ),
            "expected ReplaceBinary action"
        );
        if let Some(UpdateAction::ReplaceBinary {
            current,
            replacement,
            backup,
        }) = plan.action
        {
            assert_eq!(current, PathBuf::from("/cur/pi"));
            assert_eq!(replacement, PathBuf::from("/tmp/new-pi"));
            assert_eq!(backup, PathBuf::from("/cur/pi.bak"));
        }
        Ok(())
    }

    #[test]
    fn build_binary_plan_no_action_when_same_version() -> Result<(), UpdateError> {
        let release = LatestPiRelease {
            version: "1.0.0".to_owned(),
            package_name: None,
            note: None,
        };
        let plan = build_binary_self_update_plan(
            "1.0.0",
            "pi",
            release,
            UpdateOptions::default(),
            PathBuf::from("/cur/pi"),
            PathBuf::from("/tmp/new-pi"),
            PathBuf::from("/cur/pi.bak"),
        )?;
        assert!(!plan.should_run);
        assert!(plan.action.is_none());
        Ok(())
    }

    #[test]
    fn run_self_update_propagates_command_failure() -> Result<(), UpdateError> {
        let release = LatestPiRelease {
            version: "2.0.0".to_owned(),
            package_name: None,
            note: None,
        };
        let plan = build_self_update_plan(
            "1.0.0",
            "pi",
            release,
            InstallMethod::Npm,
            UpdateOptions::default(),
            None,
            None,
        )?;
        let mut runner = FailingRunner {
            error: Some(UpdateError::Command("npm exited with 1".to_owned())),
        };
        let result = run_self_update(&plan, UpdateOptions::default(), &mut runner);
        assert!(matches!(result, Err(UpdateError::Command(_))));
        Ok(())
    }

    #[test]
    fn run_self_update_offline_returns_error_even_with_plan() -> Result<(), UpdateError> {
        let release = LatestPiRelease {
            version: "2.0.0".to_owned(),
            package_name: None,
            note: None,
        };
        let plan = build_self_update_plan(
            "1.0.0",
            "pi",
            release,
            InstallMethod::Npm,
            UpdateOptions::default(),
            None,
            None,
        )?;
        let mut runner = RecordingRunner::default();
        let result = run_self_update(
            &plan,
            UpdateOptions {
                offline: true,
                ..UpdateOptions::default()
            },
            &mut runner,
        );
        assert!(matches!(result, Err(UpdateError::Offline)));
        assert!(runner.calls.is_empty());
        Ok(())
    }

    #[test]
    fn atomic_replace_succeeds_with_real_filesystem() -> Result<(), Box<dyn std::error::Error>> {
        use tempfile::tempdir;
        let temp = tempdir()?;
        let current = temp.path().join("pi");
        let replacement = temp.path().join("pi.new");
        let backup = temp.path().join("pi.bak");
        std::fs::write(&current, b"old")?;
        std::fs::write(&replacement, b"new")?;

        atomic_replace_binary(&StdUpdateFileSystem, &current, &replacement, &backup)?;

        assert_eq!(std::fs::read(&current)?, b"new");
        assert!(!backup.exists());
        assert!(!replacement.exists());
        Ok(())
    }

    #[test]
    fn atomic_replace_fails_when_current_missing() -> Result<(), Box<dyn std::error::Error>> {
        let temp = tempfile::tempdir()?;
        let current = temp.path().join("nonexistent");
        let replacement = temp.path().join("pi.new");
        let backup = temp.path().join("pi.bak");
        std::fs::write(&replacement, b"new")?;

        let result = atomic_replace_binary(&StdUpdateFileSystem, &current, &replacement, &backup);
        assert!(result.is_err());
        // Replacement is unchanged.
        assert_eq!(std::fs::read(&replacement)?, b"new");
        Ok(())
    }

    #[test]
    fn atomic_replace_removes_stale_backup_before_rename() -> Result<(), Box<dyn std::error::Error>>
    {
        use tempfile::tempdir;
        let temp = tempdir()?;
        let current = temp.path().join("pi");
        let replacement = temp.path().join("pi.new");
        let backup = temp.path().join("pi.bak");
        std::fs::write(&current, b"old")?;
        std::fs::write(&replacement, b"new")?;
        std::fs::write(&backup, b"stale")?;

        atomic_replace_binary(&StdUpdateFileSystem, &current, &replacement, &backup)?;

        assert_eq!(std::fs::read(&current)?, b"new");
        assert!(!backup.exists());
        Ok(())
    }

    /// Filesystem that fails `remove_file` to test backup cleanup tolerance.
    struct RemoveFileFailingFS {
        inner: StdUpdateFileSystem,
        fail_remove: bool,
    }

    impl UpdateFileSystem for RemoveFileFailingFS {
        fn exists(&self, path: &Path) -> bool {
            self.inner.exists(path)
        }
        fn rename(&self, from: &Path, to: &Path) -> std::io::Result<()> {
            self.inner.rename(from, to)
        }
        fn copy(&self, from: &Path, to: &Path) -> std::io::Result<u64> {
            self.inner.copy(from, to)
        }
        fn remove_file(&self, path: &Path) -> std::io::Result<()> {
            if self.fail_remove {
                Err(std::io::Error::new(
                    std::io::ErrorKind::PermissionDenied,
                    "locked",
                ))
            } else {
                self.inner.remove_file(path)
            }
        }
        fn remove_dir_all(&self, path: &Path) -> std::io::Result<()> {
            self.inner.remove_dir_all(path)
        }
        fn create_dir_all(&self, path: &Path) -> std::io::Result<()> {
            self.inner.create_dir_all(path)
        }
        fn permissions(&self, path: &Path) -> std::io::Result<fs::Permissions> {
            self.inner.permissions(path)
        }
        fn set_permissions(&self, path: &Path, perm: fs::Permissions) -> std::io::Result<()> {
            self.inner.set_permissions(path, perm)
        }
    }

    #[test]
    fn atomic_replace_succeeds_even_when_backup_cleanup_fails()
    -> Result<(), Box<dyn std::error::Error>> {
        use tempfile::tempdir;
        let temp = tempdir()?;
        let current = temp.path().join("pi");
        let replacement = temp.path().join("pi.new");
        let backup = temp.path().join("pi.bak");
        std::fs::write(&current, b"old")?;
        std::fs::write(&replacement, b"new")?;

        // Use a filesystem where remove_file always fails, simulating a locked backup.
        let fs = RemoveFileFailingFS {
            inner: StdUpdateFileSystem,
            fail_remove: true,
        };
        // The pre-rename stale-backup removal will fail, but since there's no stale
        // backup, the exists() check returns false and remove_file is not called.
        // After the replacement succeeds, the backup cleanup is best-effort.
        // To actually test the post-rename failure, we need the backup to exist
        // after the rename. Let's set it up so the rename creates the backup,
        // then remove_file on it fails.
        let result = atomic_replace_binary(&fs, &current, &replacement, &backup);

        // The replacement succeeded; backup cleanup failure is swallowed.
        assert!(result.is_ok());
        // Current has the new content.
        assert_eq!(std::fs::read(&current)?, b"new");
        // Backup still exists because cleanup failed (best-effort).
        assert!(backup.exists());
        assert_eq!(std::fs::read(&backup)?, b"old");
        Ok(())
    }
}
