//! Release packaging plan, runner, path safety, and reproducibility rules.
//!
//! Rust-side planner for the `scripts/package-release.ts` release builder. It
//! owns the cross-target cargo invocation, the sibling host-asset selection
//! (compiled standalone host vs Bun-runtime-plus-JavaScript fallback), the
//! archive layout, and the reproducibility contract. The actual archive bytes
//! are assembled by the release script or CI; this module makes the plan
//! deterministic, validated, and unit-testable without invoking cargo.
//!
//! Supported targets (baseline x64 hosts avoid Bun's AVX2 floor; arm64 hosts
//! use the standard target):
//! - `x86_64-unknown-linux-gnu`
//! - `aarch64-unknown-linux-gnu`
//! - `x86_64-apple-darwin`
//! - `aarch64-apple-darwin`
//! - `x86_64-pc-windows-msvc`

use std::io;
use std::path::{Path, PathBuf};

use super::command::{CommandRunner, CommandSpec};

/// A native release target triple and its host-asset branch.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub enum ReleaseTarget {
    /// Linux `x86_64` (GNU).
    LinuxX64,

    /// Linux aarch64 (GNU).
    LinuxArm64,
    /// macOS `x86_64`.
    MacosX64,
    /// macOS aarch64.
    MacosArm64,
    /// Windows `x86_64` (MSVC).
    WindowsX64,
}

impl ReleaseTarget {
    /// All supported release targets.
    #[must_use]
    pub const fn all() -> &'static [ReleaseTarget] {
        &[
            Self::LinuxX64,
            Self::LinuxArm64,
            Self::MacosX64,
            Self::MacosArm64,
            Self::WindowsX64,
        ]
    }

    /// The Rust target triple.
    #[must_use]
    pub const fn triple(self) -> &'static str {
        match self {
            Self::LinuxX64 => "x86_64-unknown-linux-gnu",
            Self::LinuxArm64 => "aarch64-unknown-linux-gnu",
            Self::MacosX64 => "x86_64-apple-darwin",
            Self::MacosArm64 => "aarch64-apple-darwin",
            Self::WindowsX64 => "x86_64-pc-windows-msvc",
        }
    }

    /// Whether the produced `pi` binary has an `.exe` suffix.
    #[must_use]
    pub const fn is_windows(self) -> bool {
        matches!(self, Self::WindowsX64)
    }

    /// Host-asset branch name. x64 targets build the baseline-x64 host (to
    /// avoid Bun's AVX2 floor); arm64 targets build the standard host.
    #[must_use]
    pub const fn host_branch(self) -> &'static str {
        match self {
            Self::LinuxX64 | Self::MacosX64 | Self::WindowsX64 => "baseline-x64",
            Self::LinuxArm64 | Self::MacosArm64 => "arm64",
        }
    }

    /// Archive extension: `.tar.gz` for Unix, `.zip` for Windows.
    #[must_use]
    pub const fn archive_extension(self) -> &'static str {
        if self.is_windows() { "zip" } else { "tar.gz" }
    }
}

/// Which host build ships beside `pi`.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub enum HostVariant {
    /// Compiled standalone host binary (`pi-host`). Preferred when the
    /// runtime-import fixture passes for the target.
    Compiled,
    /// Official Bun runtime plus the host JavaScript bundle. Fallback when the
    /// compiled host cannot run on the target.
    RuntimeFallback,
}

impl HostVariant {
    /// The host-side assets this variant contributes (excluding `pi`).
    #[must_use]
    pub fn assets(self) -> Vec<ReleaseAsset> {
        match self {
            Self::Compiled => vec![ReleaseAsset::host_compiled()],
            Self::RuntimeFallback => {
                vec![ReleaseAsset::host_runtime(), ReleaseAsset::host_script()]
            }
        }
    }
}

/// A single asset placed beside `pi` in the staging directory.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ReleaseAsset {
    /// Archive-relative path (forward slashes, no leading separator).
    pub relative_path: String,
    /// Whether the asset is an executable (normalized mode 0o755).
    pub executable: bool,
}

impl ReleaseAsset {
    /// The `pi` binary asset for `target`.
    #[must_use]
    pub fn pi_binary(target: ReleaseTarget) -> Self {
        let name = if target.is_windows() { "pi.exe" } else { "pi" };
        Self {
            relative_path: name.to_owned(),
            executable: true,
        }
    }

    /// The compiled standalone host binary asset.
    #[must_use]
    pub fn host_compiled() -> Self {
        Self {
            relative_path: "pi-host".to_owned(),
            executable: true,
        }
    }

    /// The Bun runtime asset used by the runtime-plus-JavaScript fallback.
    #[must_use]
    pub fn host_runtime() -> Self {
        Self {
            relative_path: "bun".to_owned(),
            executable: true,
        }
    }

    /// The host JavaScript bundle asset.
    #[must_use]
    pub fn host_script() -> Self {
        Self {
            relative_path: "host.js".to_owned(),
            executable: false,
        }
    }
}

/// A fully planned release for one target.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ReleasePlan {
    /// Target this plan builds.
    pub target: ReleaseTarget,
    /// Crate version being released.
    pub version: String,
    /// Which host variant ships beside the binary.
    pub host_variant: HostVariant,
    /// Cargo invocation (`cargo build -p pi --release --locked --target <triple>`).
    pub cargo_build: CommandSpec,
    /// Host branch to build alongside the binary.
    pub host_branch: &'static str,
    /// Archive base name without extension (`pi-<version>-<triple>`).
    pub archive_base: String,
    /// Archive extension (`tar.gz` or `zip`).
    pub archive_extension: &'static str,
    /// Complete asset list (pi binary plus host assets) in archive order.
    pub assets: Vec<ReleaseAsset>,
    /// Reproducibility manifest for the archive.
    pub manifest: ArchiveManifest,
}

/// Reproducibility rules for archive assembly.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ArchiveManifest {
    /// Members sorted by normalized relative path.
    pub sorted_members: Vec<ReleaseAsset>,
    /// Fixed modification timestamp (Unix seconds) applied to every member.
    pub fixed_mtime: u64,
    /// Normalized numeric owner uid.
    pub uid: u32,
    /// Normalized numeric owner gid.
    pub gid: u32,
}

/// Default fixed mtime when `SOURCE_DATE_EPOCH` is unset: the start of 2026
/// UTC, a stable reproducible anchor independent of wall-clock build time.
pub const DEFAULT_FIXED_MTIME: u64 = 1_767_225_600;

/// Resolve the fixed archive mtime from `SOURCE_DATE_EPOCH`, falling back to
/// [`DEFAULT_FIXED_MTIME`].
///
/// # Errors
///
/// Returns an error string when `SOURCE_DATE_EPOCH` is set but not a valid
/// non-negative integer.
pub fn resolved_fixed_mtime(source_date_epoch: Option<&str>) -> Result<u64, String> {
    match source_date_epoch {
        None => Ok(DEFAULT_FIXED_MTIME),
        Some(raw) => {
            let trimmed = raw.trim();
            trimmed
                .parse::<u64>()
                .map_err(|_| format!("invalid SOURCE_DATE_EPOCH: {raw:?}"))
        }
    }
}

/// Build a [`ReleasePlan`] for `target` at `version`.
///
/// `host_variant` selects the compiled host or the runtime-plus-JavaScript
/// fallback so the plan's asset list and manifest always describe the exact
/// archive that will be produced. `source_date_epoch` mirrors the
/// `SOURCE_DATE_EPOCH` environment variable (pass
/// `std::env::var("SOURCE_DATE_EPOCH").ok().as_deref()` in production); `None`
/// falls back to [`DEFAULT_FIXED_MTIME`].
///
/// # Errors
///
/// Returns an error when `SOURCE_DATE_EPOCH` is malformed, or when an asset
/// name fails [`validate_asset_name`].
pub fn plan_release(
    target: ReleaseTarget,
    version: &str,
    host_variant: HostVariant,
    source_date_epoch: Option<&str>,
) -> Result<ReleasePlan, String> {
    let fixed_mtime = resolved_fixed_mtime(source_date_epoch)?;
    let triple = target.triple();
    let cargo_build = CommandSpec::new(
        "cargo",
        [
            "build",
            "-p",
            "pi",
            "--release",
            "--locked",
            "--target",
            triple,
        ],
    );
    let mut assets = vec![ReleaseAsset::pi_binary(target)];
    assets.extend(host_variant.assets());
    assets.sort_by(|a, b| a.relative_path.cmp(&b.relative_path));
    for asset in &assets {
        validate_asset_name(&asset.relative_path)?;
    }
    let sorted_members = assets.clone();
    let archive_base = format!("pi-{version}-{triple}");
    Ok(ReleasePlan {
        target,
        version: version.to_owned(),
        host_variant,
        cargo_build,
        host_branch: target.host_branch(),
        archive_base,
        archive_extension: target.archive_extension(),
        assets,
        manifest: ArchiveManifest {
            sorted_members,
            fixed_mtime,
            uid: 0,
            gid: 0,
        },
    })
}

/// Full archive file name (`<archive_base>.<extension>`).
#[must_use]
pub fn archive_file_name(plan: &ReleasePlan) -> String {
    format!("{}.{}", plan.archive_base, plan.archive_extension)
}

/// Validate that an archive member name is safe and portable.
///
/// Rejects empty names, any path separator (`/` or `\`), leading `-`
/// (option-injection in archive tools), parent traversal (`..`), Windows drive
/// prefixes, and non-printable control characters. A safe name is a single
/// path component with no shell or archive-tool metacharacter risk.
///
/// # Errors
///
/// Returns a description of the violation.
pub fn validate_asset_name(name: &str) -> Result<(), String> {
    if name.is_empty() {
        return Err("asset name is empty".to_owned());
    }
    if name.contains('/') || name.contains('\\') {
        return Err(format!("asset name contains a path separator: {name:?}"));
    }
    if name == ".." || name.contains("..") {
        return Err(format!("asset name contains parent traversal: {name:?}"));
    }
    if name.starts_with('-') {
        return Err(format!("asset name starts with '-': {name:?}"));
    }
    if name.len() >= 2 && name.as_bytes()[1] == b':' && name.as_bytes()[0].is_ascii_alphabetic() {
        return Err(format!("asset name is a Windows drive prefix: {name:?}"));
    }
    if name.chars().any(char::is_control) {
        return Err(format!("asset name contains a control character: {name:?}"));
    }
    Ok(())
}

/// Returns `true` when `member` stays inside `staging` after normalization.
///
/// Containment is checked lexically (without following symlinks out of the
/// staging tree) so a malicious or malformed member cannot escape the archive
/// root via `..` segments.
#[must_use]
pub fn is_within_staging(member: &Path, staging: &Path) -> bool {
    let Ok(relative) = member.strip_prefix(staging) else {
        return false;
    };
    let mut depth: i32 = 0;
    for component in relative.components() {
        match component {
            std::path::Component::Normal(_) => depth += 1,
            std::path::Component::ParentDir => depth -= 1,
            std::path::Component::CurDir => {}
            // Prefix (Windows drive) or RootDir means an absolute escape.
            std::path::Component::Prefix(_) | std::path::Component::RootDir => return false,
        }
        if depth < 0 {
            return false;
        }
    }
    true
}

/// Errors produced by [`ReleaseRunner::run_build`].
#[derive(Debug, thiserror::Error)]
pub enum ReleaseError {
    /// The cargo build step failed.
    #[error("cargo build for {target} failed: {source}")]
    BuildFailed {
        /// Target triple that failed.
        target: &'static str,
        /// Underlying process error.
        #[source]
        source: io::Error,
    },
}

/// Injectable runner that executes the release plan's cargo build step.
pub trait ReleaseRunner {
    /// Run the planned cargo build for `plan`.
    ///
    /// # Errors
    ///
    /// Returns [`ReleaseError::BuildFailed`] on process failure.
    fn run_build(&mut self, plan: &ReleasePlan) -> Result<(), ReleaseError>;
}

/// Runner backed by a [`CommandRunner`].
pub struct SystemReleaseRunner {
    /// Underlying process runner.
    pub runner: Box<dyn CommandRunner>,
}

impl SystemReleaseRunner {
    /// Construct a runner over a process [`CommandRunner`].
    #[must_use]
    pub fn new(runner: Box<dyn CommandRunner>) -> Self {
        Self { runner }
    }
}

impl ReleaseRunner for SystemReleaseRunner {
    fn run_build(&mut self, plan: &ReleasePlan) -> Result<(), ReleaseError> {
        self.runner
            .run(&plan.cargo_build, None)
            .map_err(|source| ReleaseError::BuildFailed {
                target: plan.target.triple(),
                source,
            })?;
        Ok(())
    }
}

/// Resolve the on-disk path of an asset within a staging directory.
///
/// The result is always `staging.join(relative_path)`; callers should still
/// pass it through [`is_within_staging`] before reading or packing it.
#[must_use]
pub fn asset_staging_path(staging: &Path, asset: &ReleaseAsset) -> PathBuf {
    staging.join(&asset.relative_path)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::platform::command::CommandOutput;
    use std::path::Path;

    type TestResult = Result<(), Box<dyn std::error::Error>>;

    #[test]
    fn all_targets_are_unique_and_complete() {
        let triples: Vec<&str> = ReleaseTarget::all().iter().map(|t| t.triple()).collect();
        let mut dedup = triples.clone();
        dedup.sort_unstable();
        dedup.dedup();
        assert_eq!(triples.len(), 5);
        assert_eq!(dedup.len(), 5, "duplicate triples: {triples:?}");
    }

    #[test]
    fn plan_cargo_invocation_matches_contract() -> TestResult {
        let plan = plan_release(
            ReleaseTarget::LinuxX64,
            "1.2.3",
            HostVariant::Compiled,
            None,
        )
        .map_err(io::Error::other)?;
        assert_eq!(plan.cargo_build.program, "cargo");
        assert_eq!(
            plan.cargo_build.args,
            vec![
                "build",
                "-p",
                "pi",
                "--release",
                "--locked",
                "--target",
                "x86_64-unknown-linux-gnu"
            ]
        );
        assert_eq!(plan.host_variant, HostVariant::Compiled);
        assert_eq!(plan.host_branch, "baseline-x64");
        assert_eq!(plan.archive_base, "pi-1.2.3-x86_64-unknown-linux-gnu");
        assert_eq!(plan.archive_extension, "tar.gz");
        assert_eq!(
            archive_file_name(&plan),
            "pi-1.2.3-x86_64-unknown-linux-gnu.tar.gz"
        );
        Ok(())
    }

    #[test]
    fn windows_target_uses_zip_and_exe() -> TestResult {
        let plan = plan_release(
            ReleaseTarget::WindowsX64,
            "0.1.0",
            HostVariant::Compiled,
            None,
        )
        .map_err(io::Error::other)?;
        assert_eq!(plan.archive_extension, "zip");
        assert!(plan.assets.iter().any(|a| a.relative_path == "pi.exe"));
        assert_eq!(
            archive_file_name(&plan),
            "pi-0.1.0-x86_64-pc-windows-msvc.zip"
        );
        Ok(())
    }

    #[test]
    fn arm64_uses_standard_host_branch() -> TestResult {
        let plan = plan_release(
            ReleaseTarget::MacosArm64,
            "1.0.0",
            HostVariant::Compiled,
            None,
        )
        .map_err(io::Error::other)?;
        assert_eq!(plan.host_branch, "arm64");
        assert_eq!(plan.target.triple(), "aarch64-apple-darwin");
        Ok(())
    }

    #[test]
    fn runtime_fallback_swaps_host_assets() -> TestResult {
        let plan = plan_release(
            ReleaseTarget::LinuxX64,
            "1.0.0",
            HostVariant::RuntimeFallback,
            None,
        )
        .map_err(io::Error::other)?;
        let names: Vec<&str> = plan
            .assets
            .iter()
            .map(|a| a.relative_path.as_str())
            .collect();
        assert!(names.contains(&"bun"));
        assert!(names.contains(&"host.js"));
        assert!(!names.contains(&"pi-host"));
        // manifest mirrors the asset list exactly.
        assert_eq!(plan.manifest.sorted_members, plan.assets);
        Ok(())
    }

    #[test]
    fn manifest_is_sorted_and_reproducible() -> TestResult {
        let plan = plan_release(
            ReleaseTarget::LinuxX64,
            "1.0.0",
            HostVariant::Compiled,
            None,
        )
        .map_err(io::Error::other)?;
        let names: Vec<&str> = plan
            .manifest
            .sorted_members
            .iter()
            .map(|a| a.relative_path.as_str())
            .collect();
        let mut sorted = names.clone();
        sorted.sort_unstable();
        assert_eq!(names, sorted, "members must be sorted");
        assert_eq!(plan.manifest.fixed_mtime, DEFAULT_FIXED_MTIME);
        assert_eq!(plan.manifest.uid, 0);
        assert_eq!(plan.manifest.gid, 0);
        Ok(())
    }

    #[test]
    fn source_date_epoch_overrides_mtime() -> TestResult {
        let plan = plan_release(
            ReleaseTarget::LinuxX64,
            "1.0.0",
            HostVariant::Compiled,
            Some("1700000000"),
        )
        .map_err(io::Error::other)?;
        assert_eq!(plan.manifest.fixed_mtime, 1_700_000_000);
        Ok(())
    }

    #[test]
    fn malformed_source_date_epoch_is_rejected() {
        assert!(
            plan_release(
                ReleaseTarget::LinuxX64,
                "1.0.0",
                HostVariant::Compiled,
                Some("nope")
            )
            .is_err()
        );
        assert!(
            plan_release(
                ReleaseTarget::LinuxX64,
                "1.0.0",
                HostVariant::Compiled,
                Some("-5")
            )
            .is_err()
        );
    }

    #[test]
    fn validate_asset_name_rejects_traversal() {
        assert!(validate_asset_name("pi").is_ok());
        assert!(validate_asset_name("pi.exe").is_ok());
        assert!(validate_asset_name("host.js").is_ok());
        assert!(validate_asset_name("pi-host").is_ok());
        assert!(validate_asset_name("").is_err());
        assert!(validate_asset_name("../pi").is_err());
        assert!(validate_asset_name("a/b").is_err());
        assert!(validate_asset_name("a\\b").is_err());
        assert!(validate_asset_name("-x").is_err());
        assert!(validate_asset_name("C:pi").is_err());
        assert!(validate_asset_name("pi\u{0000}").is_err());
    }

    #[test]
    fn within_staging_rejects_escape() {
        let staging = Path::new("/tmp/stage");
        assert!(is_within_staging(&staging.join("pi"), staging));
        assert!(is_within_staging(
            &staging.join("dir").join("host.js"),
            staging
        ));
        assert!(!is_within_staging(&staging.join("..").join("etc"), staging));
        assert!(!is_within_staging(Path::new("/etc/passwd"), staging));
    }

    /// Command runner that always succeeds with a zero exit.
    struct OkRunner;
    impl CommandRunner for OkRunner {
        fn run(&mut self, _spec: &CommandSpec, _stdin: Option<&[u8]>) -> io::Result<CommandOutput> {
            Ok(CommandOutput {
                status: 0,
                stdout: Vec::new(),
                stderr: Vec::new(),
            })
        }
        fn spawn_detached(&mut self, _spec: &CommandSpec) -> io::Result<()> {
            Ok(())
        }
    }

    /// Command runner that always fails.
    struct FailingRunner;
    impl CommandRunner for FailingRunner {
        fn run(&mut self, _spec: &CommandSpec, _stdin: Option<&[u8]>) -> io::Result<CommandOutput> {
            Err(io::Error::other("boom"))
        }
        fn spawn_detached(&mut self, _spec: &CommandSpec) -> io::Result<()> {
            Ok(())
        }
    }

    #[test]
    fn runner_delegates_on_success() -> TestResult {
        let mut system = SystemReleaseRunner::new(Box::new(OkRunner));
        let plan = plan_release(
            ReleaseTarget::LinuxX64,
            "1.0.0",
            HostVariant::Compiled,
            None,
        )
        .map_err(io::Error::other)?;
        // The exact cargo argv is pinned by `plan_cargo_invocation_matches_contract`;
        // here we only assert the runner surfaces success from the command runner.
        system.run_build(&plan)?;
        Ok(())
    }

    #[test]
    fn runner_surfaces_build_failure() -> TestResult {
        let mut system = SystemReleaseRunner::new(Box::new(FailingRunner));
        let plan = plan_release(
            ReleaseTarget::LinuxX64,
            "1.0.0",
            HostVariant::Compiled,
            None,
        )
        .map_err(io::Error::other)?;
        let Err(err) = system.run_build(&plan) else {
            return Err(io::Error::other("expected release build failure").into());
        };
        match err {
            ReleaseError::BuildFailed { target, .. } => {
                assert_eq!(target, "x86_64-unknown-linux-gnu");
            }
        }
        Ok(())
    }

    #[test]
    fn asset_staging_path_joins_under_staging() {
        let staging = Path::new("/stage");
        let asset = ReleaseAsset::host_script();
        let path = asset_staging_path(staging, &asset);
        assert_eq!(path, Path::new("/stage/host.js"));
        assert!(is_within_staging(&path, staging));
    }
}
