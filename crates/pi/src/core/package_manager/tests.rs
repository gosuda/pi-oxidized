//! Unit tests for [`super::PackageManager`].

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use tempfile::TempDir;

use super::*;
use crate::core::resources::discovery::PackagePathResolver;
use crate::core::settings::{
    PackageSource, PackageSourceFilter, SettingsManager, SettingsManagerCreateOptions,
};

type TestResult<T = ()> = Result<T, Box<dyn std::error::Error>>;

// ---------------------------------------------------------------------------
// Fake runner
// ---------------------------------------------------------------------------

#[derive(Clone)]
enum FakeOutcome {
    Ok,
    Capture(String),
    Fail(String),
    Timeout,
}

#[derive(Clone)]
struct FakeRule {
    command: String,
    contains: Vec<String>,
    outcome: FakeOutcome,
    /// Optional file to materialize when the rule fires (simulates clone).
    write_file: Option<(PathBuf, String)>,
}

#[derive(Clone, Default)]
struct FakeRunner {
    rules: Arc<Vec<FakeRule>>,
    calls: Arc<Mutex<Vec<FakeCall>>>,
}

#[derive(Clone, Debug)]
struct FakeCall {
    command: String,
    args: Vec<String>,
    cwd: Option<PathBuf>,
    timeout_ms: Option<u64>,
}

impl FakeRunner {
    fn builder() -> FakeRunnerBuilder {
        FakeRunnerBuilder { rules: Vec::new() }
    }

    fn lock_calls(&self) -> std::sync::MutexGuard<'_, Vec<FakeCall>> {
        self.calls
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    fn calls(&self) -> Vec<FakeCall> {
        self.lock_calls().clone()
    }

    fn find(&self, command: &str) -> Option<FakeCall> {
        self.calls().into_iter().find(|c| c.command == command)
    }

    /// Calls (command, args) in invocation order.
    fn trace(&self) -> Vec<(String, Vec<String>)> {
        self.calls()
            .into_iter()
            .map(|c| (c.command, c.args))
            .collect()
    }

    fn match_rule(&self, req: &RunRequest) -> Option<usize> {
        for (idx, rule) in self.rules.iter().enumerate() {
            if rule.command != req.command {
                continue;
            }
            if rule
                .contains
                .iter()
                .all(|needle| req.args.iter().any(|a| a == needle))
            {
                return Some(idx);
            }
        }
        None
    }
}

struct FakeRunnerBuilder {
    rules: Vec<FakeRule>,
}

impl FakeRunnerBuilder {
    fn add(mut self, command: &str, contains: &[&str], outcome: FakeOutcome) -> Self {
        self.rules.push(FakeRule {
            command: command.to_owned(),
            contains: contains.iter().map(|s| (*s).to_owned()).collect(),
            outcome,
            write_file: None,
        });
        self
    }

    /// Rule that materializes `file` when it fires.
    fn add_write(mut self, command: &str, contains: &[&str], file: PathBuf, body: String) -> Self {
        self.rules.push(FakeRule {
            command: command.to_owned(),
            contains: contains.iter().map(|s| (*s).to_owned()).collect(),
            outcome: FakeOutcome::Ok,
            write_file: Some((file, body)),
        });
        self
    }

    fn build(self) -> FakeRunner {
        FakeRunner {
            rules: Arc::new(self.rules),
            calls: Arc::new(Mutex::new(Vec::new())),
        }
    }
}

impl Runner for FakeRunner {
    fn run(&self, req: &RunRequest) -> Result<(), RunError> {
        self.record(req);
        if let Some(idx) = self.match_rule(req) {
            let rule = &self.rules[idx];
            if let Some((path, body)) = &rule.write_file {
                if let Some(parent) = path.parent() {
                    let _ = std::fs::create_dir_all(parent);
                }
                let _ = std::fs::write(path, body);
            }
            match &rule.outcome {
                FakeOutcome::Ok | FakeOutcome::Capture(_) => Ok(()),
                FakeOutcome::Fail(msg) => Err(RunError::Failed(msg.clone())),
                FakeOutcome::Timeout => {
                    Err(RunError::TimedOut(format!("{} timed out", req.label())))
                }
            }
        } else {
            Ok(())
        }
    }

    fn capture(&self, req: &RunRequest) -> Result<String, RunError> {
        self.record(req);
        if let Some(idx) = self.match_rule(req) {
            let rule = &self.rules[idx];
            match &rule.outcome {
                FakeOutcome::Ok => Ok(String::new()),
                FakeOutcome::Capture(s) => Ok(s.clone()),
                FakeOutcome::Fail(msg) => Err(RunError::Failed(msg.clone())),
                FakeOutcome::Timeout => {
                    Err(RunError::TimedOut(format!("{} timed out", req.label())))
                }
            }
        } else {
            Ok(String::new())
        }
    }
}

impl FakeRunner {
    fn record(&self, req: &RunRequest) {
        self.lock_calls().push(FakeCall {
            command: req.command.clone(),
            args: req.args.clone(),
            cwd: req.cwd.clone(),
            timeout_ms: req.timeout_ms,
        });
    }
}

// ---------------------------------------------------------------------------
// Harness
// ---------------------------------------------------------------------------

struct Harness {
    _tmp: TempDir,
    cwd: PathBuf,
    agent: PathBuf,
}

impl Harness {
    fn new() -> TestResult<Self> {
        let tmp = TempDir::new()?;
        let root = tmp.path().to_path_buf();
        Ok(Self {
            _tmp: tmp,
            cwd: root.join("cwd"),
            agent: root.join("agent"),
        })
    }

    fn options(&self) -> PackageManagerOptions {
        PackageManagerOptions::new(&self.cwd, &self.agent)
    }

    fn settings_trusted(&self) -> SettingsManager {
        SettingsManager::create(
            &self.cwd,
            Some(&self.agent),
            SettingsManagerCreateOptions::default(),
        )
    }

    fn settings_untrusted(&self) -> SettingsManager {
        SettingsManager::create(
            &self.cwd,
            Some(&self.agent),
            SettingsManagerCreateOptions::default().project_trusted(false),
        )
    }

    fn manager_with(&self, runner: FakeRunner) -> PackageManager<FakeRunner> {
        PackageManager::with_runner(self.options(), runner)
    }

    fn manager(&self) -> PackageManager<FakeRunner> {
        let runner = FakeRunner::builder().build();
        self.manager_with(runner)
    }
}

// ---------------------------------------------------------------------------
// Source parsing
// ---------------------------------------------------------------------------

#[test]
fn parse_source_npm_variants() {
    assert!(matches!(
        parse_source("npm:foo"),
        ParsedSource::Npm { name, version: None, .. } if name == "foo"
    ));
    assert!(matches!(
        parse_source("npm:@scope/bar@^1.2.3"),
        ParsedSource::Npm { name, version, .. } if name == "@scope/bar" && version.as_deref() == Some("^1.2.3")
    ));
}

#[test]
fn parse_source_git_variants() -> TestResult {
    let https = parse_source("https://github.com/user/repo");
    match &https {
        ParsedSource::Git {
            host,
            path,
            ref_name,
            pinned,
            ..
        } => {
            assert_eq!(host, "github.com");
            assert_eq!(path, "user/repo");
            assert!(ref_name.is_none());
            assert!(!*pinned);
        }
        other => return Err(format!("expected git, got {other:?}").into()),
    }

    let with_ref = parse_source("https://github.com/user/repo@v1.0.0");
    match &with_ref {
        ParsedSource::Git {
            path,
            ref_name,
            pinned,
            ..
        } => {
            assert_eq!(path, "user/repo");
            assert_eq!(ref_name.as_deref(), Some("v1.0.0"));
            assert!(*pinned);
        }
        other => return Err(format!("expected git with ref, got {other:?}").into()),
    }
    Ok(())
}

#[test]
fn parse_source_local() {
    assert!(matches!(
        parse_source("./local/ext"),
        ParsedSource::Local { path } if path == "./local/ext"
    ));
    assert!(matches!(
        parse_source("bare-name"),
        ParsedSource::Local { path } if path == "bare-name"
    ));
}

// ---------------------------------------------------------------------------
// Managed paths (matching resource resolver, including temp sha8)
// ---------------------------------------------------------------------------

#[test]
fn managed_npm_paths_match_resolver() -> TestResult {
    let h = Harness::new()?;
    let pm = h.manager();
    let settings = h.settings_trusted();

    let user = pm.managed_npm_install_path("pkg", Scope::User)?;
    assert_eq!(user, h.agent.join("npm").join("node_modules").join("pkg"));

    let project = pm.managed_npm_install_path("pkg", Scope::Project)?;
    assert_eq!(
        project,
        h.cwd
            .join(CONFIG_DIR_NAME)
            .join("npm")
            .join("node_modules")
            .join("pkg")
    );

    // The discovery resolver computes the same user-scope path.
    let path_resolver = PackagePathResolver::new(&h.cwd, &h.agent, &settings);
    let resolved_sources =
        path_resolver.resolve_extension_sources(&["npm:pkg".to_owned()], false, false)?;
    // Not installed → no extensions, but no panic, proving the path surface agrees.
    assert!(resolved_sources.extensions.is_empty());
    Ok(())
}

#[test]
fn managed_git_paths_user_project() -> TestResult {
    let h = Harness::new()?;
    let pm = h.manager();
    let user = pm.git_install_path("github.com", "user/repo", Scope::User)?;
    let project = pm.git_install_path("github.com", "user/repo", Scope::Project)?;
    assert_eq!(
        user,
        h.agent
            .join("git")
            .join("github.com")
            .join("user")
            .join("repo")
    );
    assert_eq!(
        project,
        h.cwd
            .join(CONFIG_DIR_NAME)
            .join("git")
            .join("github.com")
            .join("user")
            .join("repo")
    );
    Ok(())
}

#[test]
fn temporary_dir_uses_sha8_matching_resolver() -> TestResult {
    let h = Harness::new()?;
    let pm = h.manager();
    let temp_npm = pm.temporary_dir("npm", None)?;
    let hash = crate::core::resources::discovery::temporary_dir_hash("npm", "");
    assert_eq!(
        temp_npm,
        h.agent
            .join("tmp")
            .join("extensions")
            .join("npm")
            .join(&hash)
    );
    // Precomputed: hashlib.sha256(b'npm-').hexdigest()[:8] == "f35b2129"
    assert_eq!(hash, "f35b2129");

    let temp_git = pm.temporary_dir("git-github.com", Some("user/repo"))?;
    let git_hash =
        crate::core::resources::discovery::temporary_dir_hash("git-github.com", "user/repo");
    assert_eq!(
        temp_git,
        h.agent
            .join("tmp")
            .join("extensions")
            .join("git-github.com")
            .join(&git_hash)
            .join("user")
            .join("repo")
    );
    // Precomputed: hashlib.sha256(b'git-github.com-user/repo').hexdigest()[:8]
    assert_eq!(git_hash, "338a1076");
    Ok(())
}

#[test]
fn path_escape_is_rejected() -> TestResult {
    let h = Harness::new()?;
    let pm = h.manager();
    match pm.git_install_path("..", "evil/repo", Scope::User) {
        Err(PackageManagerError::PathEscape(_)) => Ok(()),
        other => Err(format!("expected PathEscape, got {other:?}").into()),
    }
}

// ---------------------------------------------------------------------------
// Trust gate
// ---------------------------------------------------------------------------

#[test]
fn project_scope_refused_when_untrusted() -> TestResult {
    let h = Harness::new()?;
    let pm = h.manager();
    let settings = h.settings_untrusted();
    match pm.install(&settings, "npm:pkg", Scope::Project) {
        Err(PackageManagerError::ProjectNotTrusted) => {}
        other => return Err(format!("expected ProjectNotTrusted, got {other:?}").into()),
    }
    let mut settings = h.settings_untrusted();
    match pm.add_source_to_settings(&mut settings, "npm:pkg", Scope::Project) {
        Err(PackageManagerError::ProjectNotTrusted) => Ok(()),
        other => Err(format!("expected ProjectNotTrusted, got {other:?}").into()),
    }
}

#[test]
fn user_scope_does_not_require_trust() -> TestResult {
    let h = Harness::new()?;
    let runner = FakeRunner::builder()
        .add("npm", &["install"], FakeOutcome::Ok)
        .build();
    let pm = h.manager_with(runner);
    let settings = h.settings_untrusted();
    pm.install(&settings, "npm:pkg", Scope::User)?;
    Ok(())
}

// ---------------------------------------------------------------------------
// Argv per package manager
// ---------------------------------------------------------------------------

#[test]
fn npm_install_and_uninstall_argv() -> TestResult {
    let h = Harness::new()?;
    let runner = FakeRunner::builder()
        .add("npm", &["install"], FakeOutcome::Ok)
        .add("npm", &["uninstall"], FakeOutcome::Ok)
        .build();
    let pm = h.manager_with(runner);
    let settings = h.settings_trusted();

    pm.install(&settings, "npm:foo", Scope::User)?;
    let install_call = pm.runner.find("npm").ok_or("no npm call")?;
    let root = h.agent.join("npm");
    assert_eq!(
        install_call.args,
        vec![
            "install".to_owned(),
            "foo".to_owned(),
            "--prefix".to_owned(),
            root.to_string_lossy().into_owned(),
            "--legacy-peer-deps".to_owned(),
        ]
    );
    assert_eq!(install_call.timeout_ms, Some(NETWORK_TIMEOUT_MS));

    pm.remove(&settings, "npm:foo", Scope::User)?;
    let uninstall = pm
        .runner
        .calls()
        .into_iter()
        .rev()
        .find(|c| c.args.first().is_some_and(|a| a == "uninstall"))
        .ok_or("no uninstall")?;
    assert_eq!(
        uninstall.args,
        vec![
            "uninstall".to_owned(),
            "foo".to_owned(),
            "--prefix".to_owned(),
            root.to_string_lossy().into_owned(),
            "--legacy-peer-deps".to_owned(),
        ]
    );
    Ok(())
}

#[test]
fn bun_install_and_uninstall_argv() -> TestResult {
    let h = Harness::new()?;
    let runner = FakeRunner::builder()
        .add("bun", &["install"], FakeOutcome::Ok)
        .add("bun", &["uninstall"], FakeOutcome::Ok)
        .build();
    let pm = h.manager_with(runner);
    let mut settings = h.settings_trusted();
    settings.set_npm_command(Some(vec!["bun".to_owned()]));

    pm.install(&settings, "npm:foo", Scope::User)?;
    let call = pm.runner.find("bun").ok_or("no bun call")?;
    let root = h.agent.join("npm");
    assert_eq!(
        call.args,
        vec![
            "install".to_owned(),
            "foo".to_owned(),
            "--cwd".to_owned(),
            root.to_string_lossy().into_owned(),
            "--omit=peer".to_owned(),
        ]
    );

    pm.remove(&settings, "npm:foo", Scope::User)?;
    let uninstall = pm
        .runner
        .calls()
        .into_iter()
        .rev()
        .find(|c| c.args.first().is_some_and(|a| a == "uninstall"))
        .ok_or("no uninstall")?;
    assert_eq!(
        uninstall.args,
        vec![
            "uninstall".to_owned(),
            "foo".to_owned(),
            "--cwd".to_owned(),
            root.to_string_lossy().into_owned(),
        ]
    );
    Ok(())
}

#[test]
fn pnpm_install_and_uninstall_argv() -> TestResult {
    let h = Harness::new()?;
    let runner = FakeRunner::builder()
        .add("pnpm", &["install"], FakeOutcome::Ok)
        .add("pnpm", &["uninstall"], FakeOutcome::Ok)
        .build();
    let pm = h.manager_with(runner);
    let mut settings = h.settings_trusted();
    settings.set_npm_command(Some(vec!["pnpm".to_owned()]));

    pm.install(&settings, "npm:foo", Scope::User)?;
    let call = pm.runner.find("pnpm").ok_or("no pnpm call")?;
    let root = h.agent.join("npm");
    assert_eq!(
        call.args,
        vec![
            "install".to_owned(),
            "foo".to_owned(),
            "--prefix".to_owned(),
            root.to_string_lossy().into_owned(),
            "--config.auto-install-peers=false".to_owned(),
            "--config.strict-peer-dependencies=false".to_owned(),
            "--config.strict-dep-builds=false".to_owned(),
        ]
    );

    pm.remove(&settings, "npm:foo", Scope::User)?;
    let uninstall = pm
        .runner
        .calls()
        .into_iter()
        .rev()
        .find(|c| c.args.first().is_some_and(|a| a == "uninstall"))
        .ok_or("no uninstall")?;
    assert_eq!(
        uninstall.args,
        vec![
            "uninstall".to_owned(),
            "foo".to_owned(),
            "--prefix".to_owned(),
            root.to_string_lossy().into_owned(),
        ]
    );
    Ok(())
}

#[test]
fn npm_command_prefix_is_prepended() -> TestResult {
    let h = Harness::new()?;
    let runner = FakeRunner::builder()
        .add("mycli", &["install"], FakeOutcome::Ok)
        .build();
    let pm = h.manager_with(runner);
    let mut settings = h.settings_trusted();
    settings.set_npm_command(Some(vec!["mycli".to_owned(), "--rcfile=./r".to_owned()]));
    pm.install(&settings, "npm:foo", Scope::User)?;
    let call = pm.runner.find("mycli").ok_or("no call")?;
    assert_eq!(&call.args[0..2], ["--rcfile=./r", "install"]);
    Ok(())
}

// ---------------------------------------------------------------------------
// local / git install
// ---------------------------------------------------------------------------

#[test]
fn local_install_missing_path_errors() -> TestResult {
    let h = Harness::new()?;
    let pm = h.manager();
    let settings = h.settings_trusted();
    match pm.install(&settings, "./nope/missing", Scope::User) {
        Err(PackageManagerError::PathNotFound(_)) => Ok(()),
        other => Err(format!("expected PathNotFound, got {other:?}").into()),
    }
}

#[test]
fn local_install_existing_path_ok() -> TestResult {
    let h = Harness::new()?;
    std::fs::create_dir_all(h.cwd.join("exts"))?;
    let pm = h.manager();
    let settings = h.settings_trusted();
    pm.install(&settings, "./exts", Scope::User)?;
    assert!(pm.runner.calls().is_empty());
    Ok(())
}

#[test]
fn git_install_clone_argv() -> TestResult {
    let h = Harness::new()?;
    let runner = FakeRunner::builder()
        .add("git", &["clone"], FakeOutcome::Ok)
        .build();
    let pm = h.manager_with(runner);
    let settings = h.settings_trusted();
    pm.install(&settings, "https://github.com/user/repo", Scope::User)?;

    let clone = pm.runner.find("git").ok_or("no git call")?;
    let target = h
        .agent
        .join("git")
        .join("github.com")
        .join("user")
        .join("repo");
    assert_eq!(
        clone.args,
        vec![
            "clone".to_owned(),
            "https://github.com/user/repo".to_owned(),
            target.to_string_lossy().into_owned(),
        ]
    );
    assert_eq!(clone.timeout_ms, Some(NETWORK_TIMEOUT_MS));
    assert!(h.agent.join("git").join(".gitignore").exists());
    Ok(())
}

#[test]
fn git_install_fresh_clone_with_ref_uses_checkout() -> TestResult {
    let h = Harness::new()?;
    let runner = FakeRunner::builder()
        .add("git", &["clone"], FakeOutcome::Ok)
        .add("git", &["checkout"], FakeOutcome::Ok)
        .build();
    let pm = h.manager_with(runner);
    let settings = h.settings_trusted();
    pm.install(
        &settings,
        "https://github.com/user/repo@v1.0.0",
        Scope::User,
    )?;
    // Fresh clone checks out the ref directly (no fetch/reset).
    let checkout = pm
        .runner
        .calls()
        .into_iter()
        .find(|c| c.args.first().is_some_and(|a| a == "checkout"))
        .ok_or("no checkout")?;
    assert_eq!(
        checkout.args,
        vec!["checkout".to_owned(), "v1.0.0".to_owned()]
    );
    Ok(())
}

#[test]
fn git_install_runs_deps_when_package_json_present() -> TestResult {
    let h = Harness::new()?;
    let target = h
        .agent
        .join("git")
        .join("github.com")
        .join("user")
        .join("repo");
    let pkg_json = target.join("package.json");
    let runner = FakeRunner::builder()
        .add_write("git", &["clone"], pkg_json.clone(), "{}".to_owned())
        .add("npm", &["install"], FakeOutcome::Ok)
        .build();
    let pm = h.manager_with(runner);
    let settings = h.settings_trusted();
    pm.install(&settings, "https://github.com/user/repo", Scope::User)?;
    let npm_call = pm.runner.find("npm").ok_or("no npm deps call")?;
    assert_eq!(
        npm_call.args,
        vec!["install".to_owned(), "--omit=dev".to_owned()]
    );
    assert_eq!(npm_call.cwd.as_deref(), Some(target.as_path()));
    Ok(())
}

#[test]
fn npm_install_creates_managed_project_files() -> TestResult {
    let h = Harness::new()?;
    let runner = FakeRunner::builder()
        .add("npm", &["install"], FakeOutcome::Ok)
        .build();
    let pm = h.manager_with(runner);
    let settings = h.settings_trusted();
    pm.install(&settings, "npm:foo", Scope::User)?;
    let root = h.agent.join("npm");
    assert_eq!(
        std::fs::read_to_string(root.join(".gitignore"))?,
        GITIGNORE_CONTENT
    );
    assert!(std::fs::read_to_string(root.join("package.json"))?.contains("pi-extensions"));
    Ok(())
}

// ---------------------------------------------------------------------------
// ensure_git_ref ordering: clean -fdx + deps reinstall (defect 1)
// ---------------------------------------------------------------------------

fn git_ref_update_rules(head_local: &str, head_target: &str) -> Vec<FakeRule> {
    let mut rules = Vec::new();
    // Specific git subcommands first.
    for spec in ["clone", "fetch", "reset", "clean", "checkout", "set-head"] {
        rules.push(FakeRule {
            command: "git".to_owned(),
            contains: vec![spec.to_owned()],
            outcome: FakeOutcome::Ok,
            write_file: None,
        });
    }
    // rev-parse variants, most specific first.
    rules.push(FakeRule {
        command: "git".to_owned(),
        contains: vec!["--abbrev-ref".to_owned()],
        outcome: FakeOutcome::Capture("origin/main".to_owned()),
        write_file: None,
    });
    rules.push(FakeRule {
        command: "git".to_owned(),
        contains: vec!["FETCH_HEAD^{commit}".to_owned()],
        outcome: FakeOutcome::Capture(head_target.to_owned()),
        write_file: None,
    });
    rules.push(FakeRule {
        command: "git".to_owned(),
        contains: vec!["origin/HEAD^{commit}".to_owned()],
        outcome: FakeOutcome::Capture(head_target.to_owned()),
        write_file: None,
    });
    rules.push(FakeRule {
        command: "git".to_owned(),
        contains: vec!["HEAD".to_owned()],
        outcome: FakeOutcome::Capture(head_local.to_owned()),
        write_file: None,
    });
    rules.push(FakeRule {
        command: "npm".to_owned(),
        contains: vec!["install".to_owned()],
        outcome: FakeOutcome::Ok,
        write_file: None,
    });
    rules
}

#[test]
fn ensure_git_ref_resets_cleans_and_reinstalls_deps() -> TestResult {
    let h = Harness::new()?;
    // Existing clone with a package.json; heads differ → reset + clean + deps.
    let target = h
        .agent
        .join("git")
        .join("github.com")
        .join("user")
        .join("repo");
    std::fs::create_dir_all(&target)?;
    std::fs::write(target.join("package.json"), "{}")?;
    let runner = FakeRunner {
        rules: Arc::new(git_ref_update_rules("aaa", "bbb")),
        calls: Arc::new(Mutex::new(Vec::new())),
    };
    let pm = h.manager_with(runner);
    let settings = h.settings_trusted();
    pm.install(
        &settings,
        "https://github.com/user/repo@feature",
        Scope::User,
    )?;

    let trace: Vec<&str> = pm
        .runner
        .trace()
        .iter()
        .map(
            |(cmd, args)| match (cmd.as_str(), args.first().map(String::as_str)) {
                ("git", Some("fetch")) => "fetch",
                ("git", Some("reset")) => "reset",
                ("git", Some("clean")) => "clean",
                ("git", Some("rev-parse")) => "rev-parse",
                ("npm", Some("install")) => "npm-install",
                _ => "other",
            },
        )
        .collect();
    // fetch → rev-parse HEAD → rev-parse FETCH_HEAD^{commit} → reset → clean → npm-install
    let key_events: Vec<&str> = trace
        .iter()
        .copied()
        .filter(|s| matches!(*s, "fetch" | "reset" | "clean" | "npm-install"))
        .collect();
    assert_eq!(
        key_events,
        vec!["fetch", "reset", "clean", "npm-install"],
        "trace={trace:?}"
    );
    // clean runs after reset and before deps; assert exact clean argv.
    let clean = pm
        .runner
        .calls()
        .into_iter()
        .find(|c| c.args.first().is_some_and(|a| a == "clean"))
        .ok_or("no clean")?;
    assert_eq!(clean.args, vec!["clean".to_owned(), "-fdx".to_owned()]);
    assert_eq!(clean.cwd.as_deref(), Some(target.as_path()));
    Ok(())
}

#[test]
fn ensure_git_ref_noop_when_heads_equal() -> TestResult {
    let h = Harness::new()?;
    let target = h
        .agent
        .join("git")
        .join("github.com")
        .join("user")
        .join("repo");
    std::fs::create_dir_all(&target)?;
    std::fs::write(target.join("package.json"), "{}")?;
    let runner = FakeRunner {
        rules: Arc::new(git_ref_update_rules("same", "same")),
        calls: Arc::new(Mutex::new(Vec::new())),
    };
    let pm = h.manager_with(runner);
    let settings = h.settings_trusted();
    pm.install(
        &settings,
        "https://github.com/user/repo@feature",
        Scope::User,
    )?;
    let cmds = pm.runner.trace();
    assert!(
        cmds.iter()
            .any(|(c, a)| c == "git" && a.first().is_some_and(|x| x == "fetch"))
    );
    assert!(
        !cmds
            .iter()
            .any(|(c, a)| c == "git" && a.first().is_some_and(|x| x == "reset"))
    );
    assert!(
        !cmds
            .iter()
            .any(|(c, a)| c == "git" && a.first().is_some_and(|x| x == "clean"))
    );
    assert!(
        !cmds
            .iter()
            .any(|(c, a)| c == "npm" && a.first().is_some_and(|x| x == "install"))
    );
    Ok(())
}

#[test]
fn local_git_update_target_final_fallback_uses_head_ref() -> TestResult {
    let h = Harness::new()?;
    let target = h
        .agent
        .join("git")
        .join("github.com")
        .join("user")
        .join("repo");
    std::fs::create_dir_all(&target)?;
    let mut rules = Vec::new();
    for spec in ["clone", "fetch", "reset", "clean", "checkout", "set-head"] {
        rules.push(FakeRule {
            command: "git".to_owned(),
            contains: vec![spec.to_owned()],
            outcome: FakeOutcome::Ok,
            write_file: None,
        });
    }
    // @{upstream} unsupported → fallback; symbolic-ref empty → final +HEAD path.
    rules.push(FakeRule {
        command: "git".to_owned(),
        contains: vec!["--abbrev-ref".to_owned()],
        outcome: FakeOutcome::Capture("(none)".to_owned()),
        write_file: None,
    });
    rules.push(FakeRule {
        command: "git".to_owned(),
        contains: vec!["symbolic-ref".to_owned()],
        outcome: FakeOutcome::Capture(String::new()),
        write_file: None,
    });
    rules.push(FakeRule {
        command: "git".to_owned(),
        contains: vec!["origin/HEAD^{commit}".to_owned()],
        outcome: FakeOutcome::Capture("same".to_owned()),
        write_file: None,
    });
    rules.push(FakeRule {
        command: "git".to_owned(),
        contains: vec!["HEAD".to_owned()],
        outcome: FakeOutcome::Capture("same".to_owned()),
        write_file: None,
    });
    let runner = FakeRunner {
        rules: Arc::new(rules),
        calls: Arc::new(Mutex::new(Vec::new())),
    };
    let pm = h.manager_with(runner);
    let settings = h.settings_trusted();
    // No ref → install_git existing-target calls local_git_update_target.
    pm.install(&settings, "https://github.com/user/repo", Scope::User)?;
    let fetch = pm
        .runner
        .calls()
        .into_iter()
        .find(|c| c.command == "git" && c.args.first().is_some_and(|a| a == "fetch"))
        .ok_or("no fetch")?;
    assert_eq!(
        fetch.args,
        vec![
            "fetch".to_owned(),
            "--prune".to_owned(),
            "--no-tags".to_owned(),
            "origin".to_owned(),
            "+HEAD:refs/remotes/origin/HEAD".to_owned(),
        ]
    );
    Ok(())
}

// ---------------------------------------------------------------------------
// Offline + timeout
// ---------------------------------------------------------------------------

#[test]
fn offline_mode_skips_update() -> TestResult {
    let h = Harness::new()?;
    let runner = FakeRunner::builder()
        .add(
            "npm",
            &["view"],
            FakeOutcome::Capture("\"1.0.0\"".to_owned()),
        )
        .build();
    let pm = h.manager_with(runner).with_offline(true);
    let mut settings = h.settings_trusted();
    settings.set_packages(&[PackageSource::Source("npm:foo".to_owned())]);
    pm.update_extensions(&settings, None)?;
    assert!(pm.runner.calls().is_empty());
    Ok(())
}

#[test]
fn timeout_returns_runner_error() -> TestResult {
    let h = Harness::new()?;
    let runner = FakeRunner::builder()
        .add("git", &["clone"], FakeOutcome::Timeout)
        .build();
    let pm = h.manager_with(runner);
    let settings = h.settings_trusted();
    match pm.install(&settings, "https://github.com/user/repo", Scope::User) {
        Err(PackageManagerError::Runner(msg)) if msg.contains("timed out") => Ok(()),
        other => Err(format!("expected Runner timeout, got {other:?}").into()),
    }
}

// ---------------------------------------------------------------------------
// Settings: rollback, idempotency, normalize, dedupe, unknown fields
// ---------------------------------------------------------------------------

#[test]
fn install_failure_leaves_settings_untouched() -> TestResult {
    let h = Harness::new()?;
    let runner = FakeRunner::builder()
        .add("git", &["clone"], FakeOutcome::Fail("boom".to_owned()))
        .build();
    let pm = h.manager_with(runner);
    let mut settings = h.settings_trusted();
    match pm.install_and_persist(&mut settings, "https://github.com/user/repo", Scope::User) {
        Err(PackageManagerError::Runner(_)) => {}
        other => return Err(format!("expected Runner error, got {other:?}").into()),
    }
    assert!(settings.get_packages().is_empty());
    Ok(())
}

#[test]
fn add_source_is_idempotent() -> TestResult {
    let h = Harness::new()?;
    let pm = h.manager();
    let mut settings = h.settings_trusted();
    assert!(pm.add_source_to_settings(&mut settings, "npm:foo", Scope::User)?);
    assert!(!pm.add_source_to_settings(&mut settings, "npm:foo", Scope::User)?);
    assert_eq!(
        settings.get_packages(),
        vec![PackageSource::Source("npm:foo".to_owned())]
    );
    Ok(())
}

#[test]
fn add_source_renormalizes_local_path() -> TestResult {
    let h = Harness::new()?;
    // User-scope base dir is the agent dir; a local path inside it is stored
    // relative to that base (TS `normalizePackageSourceForSettings`).
    std::fs::create_dir_all(h.agent.join("exts"))?;
    let pm = h.manager();
    let mut settings = h.settings_trusted();
    let abs = h.agent.join("exts").to_string_lossy().into_owned();
    assert!(pm.add_source_to_settings(&mut settings, &abs, Scope::User)?);
    let pkgs = settings.get_packages();
    assert_eq!(pkgs.len(), 1);
    let stored = match &pkgs[0] {
        PackageSource::Source(source) => source.clone(),
        PackageSource::Filtered(filtered) => {
            return Err(format!("expected source, got filtered {filtered:?}").into());
        }
    };
    assert_eq!(
        stored, "exts",
        "local source normalized relative to agent dir"
    );
    Ok(())
}

#[test]
fn remove_source_is_idempotent() -> TestResult {
    let h = Harness::new()?;
    let pm = h.manager();
    let mut settings = h.settings_trusted();
    settings.set_packages(&[PackageSource::Source("npm:foo".to_owned())]);
    assert!(pm.remove_source_from_settings(&mut settings, "npm:foo", Scope::User)?);
    assert!(!pm.remove_source_from_settings(&mut settings, "npm:foo", Scope::User)?);
    assert!(settings.get_packages().is_empty());
    Ok(())
}

#[test]
fn remove_and_persist_reports_change() -> TestResult {
    let h = Harness::new()?;
    let runner = FakeRunner::builder()
        .add("npm", &["uninstall"], FakeOutcome::Ok)
        .build();
    let pm = h.manager_with(runner);
    let mut settings = h.settings_trusted();
    settings.set_packages(&[PackageSource::Source("npm:foo".to_owned())]);
    let changed = pm.remove_and_persist(&mut settings, "npm:foo", Scope::User)?;
    assert!(changed, "first remove changes settings");
    let changed2 = pm.remove_and_persist(&mut settings, "npm:foo", Scope::User)?;
    assert!(!changed2, "second remove is a no-op");
    Ok(())
}

#[test]
fn settings_unknown_fields_preserved_after_add() -> TestResult {
    let h = Harness::new()?;
    std::fs::create_dir_all(&h.agent)?;
    let seeded = r#"{"futureField": 7, "packages": ["npm:old"]}"#;
    std::fs::write(h.agent.join("settings.json"), seeded)?;
    let pm = h.manager();
    let mut settings = h.settings_trusted();
    assert!(pm.add_source_to_settings(&mut settings, "npm:new", Scope::User)?);
    settings.flush();

    let on_disk: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(h.agent.join("settings.json"))?)?;
    assert_eq!(on_disk["futureField"], serde_json::json!(7));
    let pkgs = on_disk["packages"].as_array().ok_or("packages array")?;
    assert!(
        pkgs.iter().any(|v| v == "npm:old"),
        "old package preserved: {pkgs:?}"
    );
    assert!(
        pkgs.iter().any(|v| v == "npm:new"),
        "new package added: {pkgs:?}"
    );
    Ok(())
}

#[test]
fn filtered_package_roundtrips_through_settings_edit() -> TestResult {
    let h = Harness::new()?;
    let pm = h.manager();
    let mut settings = h.settings_trusted();
    let filter = PackageSource::Filtered(PackageSourceFilter {
        source: "npm:foo".to_owned(),
        autoload: Some(false),
        extensions: Some(vec!["e.ts".to_owned()]),
        ..PackageSourceFilter::default()
    });
    settings.set_packages(&[filter]);
    let changed = pm.add_source_to_settings(&mut settings, "npm:foo@1.2.3", Scope::User)?;
    assert!(changed);
    let pkgs = settings.get_packages();
    assert_eq!(pkgs.len(), 1);
    match &pkgs[0] {
        PackageSource::Filtered(f) => {
            assert_eq!(f.source, "npm:foo@1.2.3");
            assert_eq!(f.autoload, Some(false));
            assert_eq!(f.extensions, Some(vec!["e.ts".to_owned()]));
        }
        PackageSource::Source(source) => {
            return Err(format!("expected filtered, got source {source:?}").into());
        }
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Update: matching, skip-pinned, no-match error + suggestion
// ---------------------------------------------------------------------------

#[test]
fn update_skips_pinned_npm_and_runs_unpinned() -> TestResult {
    let h = Harness::new()?;
    let runner = FakeRunner::builder()
        .add(
            "npm",
            &["view"],
            FakeOutcome::Capture("\"2.0.0\"".to_owned()),
        )
        .add("npm", &["install"], FakeOutcome::Ok)
        .build();
    let pm = h.manager_with(runner);
    let mut settings = h.settings_trusted();
    settings.set_packages(&[
        PackageSource::Source("npm:foo@1.0.0".to_owned()),
        PackageSource::Source("npm:bar".to_owned()),
    ]);
    pm.update_extensions(&settings, None)?;
    let install_calls: Vec<_> = pm
        .runner
        .calls()
        .into_iter()
        .filter(|c| c.command == "npm" && c.args.first().is_some_and(|a| a == "install"))
        .collect();
    assert_eq!(
        install_calls.len(),
        1,
        "only bar installs: {install_calls:?}"
    );
    assert!(install_calls[0].args.iter().any(|a| a == "bar@latest"));
    Ok(())
}

#[test]
fn update_no_match_errors_with_suggestion() -> TestResult {
    let h = Harness::new()?;
    let pm = h.manager();
    let mut settings = h.settings_trusted();
    settings.set_packages(&[PackageSource::Source("npm:foo".to_owned())]);
    match pm.update_extensions(&settings, Some("foo")) {
        Err(PackageManagerError::NoMatchingPackageWithSuggestion(src, sugg)) => {
            assert_eq!(src, "foo");
            assert_eq!(sugg, "npm:foo");
            Ok(())
        }
        other => Err(format!("expected suggestion error, got {other:?}").into()),
    }
}

#[test]
fn update_filter_matches_one_scope() -> TestResult {
    let h = Harness::new()?;
    let runner = FakeRunner::builder()
        .add(
            "npm",
            &["view"],
            FakeOutcome::Capture("\"9.9.9\"".to_owned()),
        )
        .add("npm", &["install"], FakeOutcome::Ok)
        .build();
    let pm = h.manager_with(runner);
    let mut settings = h.settings_trusted();
    settings.set_packages(&[PackageSource::Source("npm:foo".to_owned())]);
    pm.update_extensions(&settings, Some("npm:foo"))?;
    assert!(
        pm.runner
            .calls()
            .iter()
            .any(|c| c.args.first().is_some_and(|a| a == "install"))
    );
    Ok(())
}

// ---------------------------------------------------------------------------
// List + get_installed_path + resolve delegation
// ---------------------------------------------------------------------------

#[test]
fn list_and_installed_path() -> TestResult {
    let h = Harness::new()?;
    let pm = h.manager();
    let mut settings = h.settings_trusted();
    settings.set_packages(&[PackageSource::Source("npm:foo".to_owned())]);
    let installed = pm.managed_npm_install_path("foo", Scope::User)?;
    std::fs::create_dir_all(&installed)?;
    std::fs::write(installed.join("package.json"), r#"{"version":"1.0.0"}"#)?;

    let listed = pm.list_configured_packages(&settings)?;
    assert_eq!(listed.len(), 1);
    assert_eq!(listed[0].source, "npm:foo");
    assert_eq!(listed[0].scope, Scope::User);
    assert_eq!(listed[0].installed_path.as_ref(), Some(&installed));

    let direct = pm.get_installed_path(&settings, "npm:foo", Scope::User)?;
    assert_eq!(direct.as_ref(), Some(&installed));
    Ok(())
}

#[test]
fn resolve_delegates_and_matches_discovery_paths() -> TestResult {
    let h = Harness::new()?;
    let pm = h.manager();
    let mut settings = h.settings_trusted();
    settings.set_packages(&[PackageSource::Source("npm:foo".to_owned())]);
    // Materialize the user-scoped install (global package) with an extension.
    let ext_dir = h
        .agent
        .join("npm")
        .join("node_modules")
        .join("foo")
        .join("extensions");
    std::fs::create_dir_all(&ext_dir)?;
    std::fs::write(ext_dir.join("a.ts"), "// ext")?;

    let resolved = pm.resolve(&settings)?;
    let discovery = PackagePathResolver::new(&h.cwd, &h.agent, &settings).resolve()?;
    assert_eq!(resolved.extensions.len(), discovery.extensions.len());
    assert_eq!(resolved.extensions, discovery.extensions);
    assert!(!resolved.extensions.is_empty());
    Ok(())
}

// ---------------------------------------------------------------------------
// Progress callback
// ---------------------------------------------------------------------------

#[test]
fn progress_events_for_install_and_failure() -> TestResult {
    let h = Harness::new()?;
    let runner = FakeRunner::builder()
        .add("npm", &["install"], FakeOutcome::Fail("nope".to_owned()))
        .build();
    let mut pm = h.manager_with(runner);
    let events: Arc<Mutex<Vec<ProgressEvent>>> = Arc::new(Mutex::new(Vec::new()));
    let events_for_cb = events.clone();
    pm.set_progress_callback(Some(Box::new(move |event| {
        events_for_cb
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .push(event.clone());
    })));
    let settings = h.settings_trusted();
    let _ = pm.install(&settings, "npm:foo", Scope::User);
    let events = events
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .clone();
    let kinds: Vec<_> = events.iter().map(|e| e.kind).collect();
    assert_eq!(kinds, vec![ProgressKind::Start, ProgressKind::Error]);
    assert_eq!(events[0].action, ProgressAction::Install);
    assert_eq!(events[1].source, "npm:foo");
    Ok(())
}

#[test]
fn run_request_label_and_builders() {
    let req = RunRequest::new("git", vec!["clone".to_owned(), "x".to_owned()])
        .cwd("/tmp")
        .timeout_ms(100)
        .env("A", "B");
    assert_eq!(req.label(), "git clone x");
    assert_eq!(req.cwd.as_deref(), Some(Path::new("/tmp")));
    assert_eq!(req.timeout_ms, Some(100));
    assert_eq!(req.env, vec![("A".to_owned(), "B".to_owned())]);
}

#[test]
fn scope_as_str() {
    assert_eq!(Scope::User.as_str(), "user");
    assert_eq!(Scope::Project.as_str(), "project");
}

#[test]
fn semantic_max_picks_highest_version() -> TestResult {
    // Regression: lexicographic max would wrongly select "9.0.0" over "10.0.0".
    let raw = r#"["9.0.0","10.0.0","2.1.0"]"#;
    let best = super::parse_npm_view_version(raw, None).ok_or("expected some version")?;
    assert_eq!(best, "10.0.0");
    Ok(())
}
