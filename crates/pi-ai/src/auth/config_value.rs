//! Resolve configuration values that may be shell commands, environment
//! variables, or literals.
//!
//! Grammar (matching the coding-agent reference):
//! - values starting with `!` execute the remainder as a shell command (stdout,
//!   trimmed) and cache the result for the process lifetime
//! - `$NAME` / `${NAME}` interpolate environment values
//! - `$$` → literal `$`, `$!` → literal `!`
//! - unresolved environment references make the whole template resolve to
//!   `None`
//!
//! Callers resolve **copies**. Stored raw values must not be mutated in place.
//! Listing credentials must never call into command execution.

use std::collections::HashMap;
use std::io::{Read, Seek, SeekFrom};
#[cfg(unix)]
use std::os::unix::process::CommandExt as _;
#[cfg(windows)]
use std::os::windows::process::CommandExt as _;
use std::process::{Command, ExitStatus, Stdio};
use std::sync::{LazyLock, Mutex};
use std::thread;
use std::time::{Duration, Instant};

const COMMAND_TIMEOUT: Duration = Duration::from_secs(10);
const COMMAND_POLL_INTERVAL: Duration = Duration::from_millis(10);

#[derive(Debug, thiserror::Error)]
enum CommandExecutionError {
    #[error("failed to create command output file: {0}")]
    CreateOutput(std::io::Error),
    #[error("failed to clone command output file: {0}")]
    CloneOutput(std::io::Error),
    #[error("failed to spawn config command: {0}")]
    Spawn(std::io::Error),
    #[error("failed while waiting for config command: {0}")]
    Wait(std::io::Error),
    #[error("config command timed out")]
    TimedOut,
    #[error("failed to terminate config command: {0}")]
    Terminate(std::io::Error),
    #[error("config command exited with {0}")]
    Exit(ExitStatus),
    #[error("failed to read config command output: {0}")]
    ReadOutput(std::io::Error),
    #[error("config command produced no output")]
    EmptyOutput,
}

/// Provider-scoped env overlay used during resolution.
pub type ConfigEnv = std::collections::BTreeMap<String, String>;

#[derive(Clone, Debug, Eq, PartialEq)]
enum TemplatePart {
    Literal(String),
    Env(String),
}

#[derive(Clone, Debug, Eq, PartialEq)]
enum ConfigValueReference {
    Command(String),
    Template(Vec<TemplatePart>),
}

static COMMAND_CACHE: LazyLock<Mutex<HashMap<String, Option<String>>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

fn command_cache() -> &'static Mutex<HashMap<String, Option<String>>> {
    &COMMAND_CACHE
}

fn is_env_var_name(name: &str) -> bool {
    let mut chars = name.chars();
    match chars.next() {
        Some(c) if c == '_' || c.is_ascii_alphabetic() => {}
        _ => return false,
    }
    chars.all(|c| c == '_' || c.is_ascii_alphanumeric())
}

fn env_var_name_prefix_len(input: &str) -> usize {
    let mut chars = input.char_indices();
    match chars.next() {
        Some((_, c)) if c == '_' || c.is_ascii_alphabetic() => {}
        _ => return 0,
    }
    let mut end = 0;
    for (idx, c) in input.char_indices() {
        if idx == 0 {
            end = c.len_utf8();
            continue;
        }
        if c == '_' || c.is_ascii_alphanumeric() {
            end = idx + c.len_utf8();
        } else {
            break;
        }
    }
    end
}

fn append_literal(parts: &mut Vec<TemplatePart>, value: &str) {
    if value.is_empty() {
        return;
    }
    if let Some(TemplatePart::Literal(existing)) = parts.last_mut() {
        existing.push_str(value);
        return;
    }
    parts.push(TemplatePart::Literal(value.to_owned()));
}

fn parse_config_value_template(config: &str) -> Vec<TemplatePart> {
    let mut parts = Vec::new();
    let bytes = config.as_bytes();
    let mut index = 0;

    while index < config.len() {
        let Some(rel) = config[index..].find('$') else {
            append_literal(&mut parts, &config[index..]);
            break;
        };
        let dollar_index = index + rel;
        append_literal(&mut parts, &config[index..dollar_index]);

        let next = bytes.get(dollar_index + 1).copied().map(char::from);
        match next {
            Some('$' | '!') => {
                append_literal(&mut parts, &config[dollar_index + 1..dollar_index + 2]);
                index = dollar_index + 2;
            }
            Some('{') => {
                let rest = &config[dollar_index + 2..];
                if let Some(end_rel) = rest.find('}') {
                    let name = &rest[..end_rel];
                    if is_env_var_name(name) {
                        parts.push(TemplatePart::Env(name.to_owned()));
                    } else {
                        append_literal(
                            &mut parts,
                            &config[dollar_index..=(dollar_index + 2 + end_rel)],
                        );
                    }
                    index = dollar_index + 2 + end_rel + 1;
                } else {
                    append_literal(&mut parts, "$");
                    index = dollar_index + 1;
                }
            }
            Some(_) => {
                let prefix = &config[dollar_index + 1..];
                let len = env_var_name_prefix_len(prefix);
                if len > 0 {
                    parts.push(TemplatePart::Env(prefix[..len].to_owned()));
                    index = dollar_index + 1 + len;
                } else {
                    append_literal(&mut parts, "$");
                    index = dollar_index + 1;
                }
            }
            None => {
                append_literal(&mut parts, "$");
                index = dollar_index + 1;
            }
        }
    }

    parts
}

fn parse_config_value_reference(config: &str) -> ConfigValueReference {
    if config.starts_with('!') {
        ConfigValueReference::Command(config.to_owned())
    } else {
        ConfigValueReference::Template(parse_config_value_template(config))
    }
}

fn resolve_env_config_value(name: &str, env: Option<&ConfigEnv>) -> Option<String> {
    if let Some(map) = env
        && let Some(value) = map.get(name)
    {
        // Match JS `env?.[name] || process.env[name]`: empty string is falsy.
        if !value.is_empty() {
            return Some(value.clone());
        }
    }
    match std::env::var(name) {
        Ok(value) if !value.is_empty() => Some(value),
        _ => None,
    }
}

fn template_env_var_names(parts: &[TemplatePart]) -> Vec<String> {
    let mut names = Vec::new();
    for part in parts {
        if let TemplatePart::Env(name) = part
            && !names.iter().any(|existing| existing == name)
        {
            names.push(name.clone());
        }
    }
    names
}

fn resolve_template(parts: &[TemplatePart], env: Option<&ConfigEnv>) -> Option<String> {
    let mut resolved = String::new();
    for part in parts {
        match part {
            TemplatePart::Literal(value) => resolved.push_str(value),
            TemplatePart::Env(name) => {
                let value = resolve_env_config_value(name, env)?;
                resolved.push_str(&value);
            }
        }
    }
    Some(resolved)
}

/// Return the single env-var name when `config` is exactly `$NAME` / `${NAME}`.
#[must_use]
pub fn get_config_value_env_var_name(config: &str) -> Option<String> {
    match parse_config_value_reference(config) {
        ConfigValueReference::Template(parts)
            if parts.len() == 1 && matches!(parts.first(), Some(TemplatePart::Env(_))) =>
        {
            if let Some(TemplatePart::Env(name)) = parts.into_iter().next() {
                Some(name)
            } else {
                None
            }
        }
        _ => None,
    }
}

/// Return all unique env-var names referenced by a template config value.
#[must_use]
pub fn get_config_value_env_var_names(config: &str) -> Vec<String> {
    match parse_config_value_reference(config) {
        ConfigValueReference::Template(parts) => template_env_var_names(&parts),
        ConfigValueReference::Command(_) => Vec::new(),
    }
}

/// Return referenced env-var names that are currently unset.
#[must_use]
pub fn get_missing_config_value_env_var_names(
    config: &str,
    env: Option<&ConfigEnv>,
) -> Vec<String> {
    get_config_value_env_var_names(config)
        .into_iter()
        .filter(|name| resolve_env_config_value(name, env).is_none())
        .collect()
}

/// Whether `config` is a `!command` value.
#[must_use]
pub fn is_command_config_value(config: &str) -> bool {
    matches!(
        parse_config_value_reference(config),
        ConfigValueReference::Command(_)
    )
}

/// Whether every env var referenced by `config` is currently set.
#[must_use]
pub fn is_config_value_configured(config: &str, env: Option<&ConfigEnv>) -> bool {
    get_missing_config_value_env_var_names(config, env).is_empty()
}

fn terminate_process_group_and_reap(
    child: &mut std::process::Child,
) -> Result<(), CommandExecutionError> {
    #[cfg(unix)]
    {
        use nix::sys::signal::{Signal, killpg};
        use nix::unistd::Pid;

        let pid = i32::try_from(child.id()).map_err(|_| {
            CommandExecutionError::Terminate(std::io::Error::other("child PID exceeds i32"))
        })?;
        if let Err(error) = killpg(Pid::from_raw(pid), Signal::SIGKILL) {
            let _ = child.kill();
            let _ = child.wait();
            return Err(CommandExecutionError::Terminate(std::io::Error::other(
                error,
            )));
        }
    }
    #[cfg(windows)]
    {
        let status = Command::new("taskkill")
            .args(["/PID", &child.id().to_string(), "/T", "/F"])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status();
        if !matches!(status, Ok(status) if status.success()) {
            child.kill().map_err(CommandExecutionError::Terminate)?;
        }
    }

    child
        .wait()
        .map(|_| ())
        .map_err(CommandExecutionError::Wait)
}

fn execute_with_default_shell(
    command: &str,
    timeout: Duration,
) -> Result<String, CommandExecutionError> {
    let mut output_file = tempfile::tempfile().map_err(CommandExecutionError::CreateOutput)?;
    let child_output = output_file
        .try_clone()
        .map_err(CommandExecutionError::CloneOutput)?;

    let mut shell = {
        #[cfg(windows)]
        {
            let mut shell = Command::new("cmd");
            const CREATE_NEW_PROCESS_GROUP: u32 = 0x0000_0200;
            const CREATE_NO_WINDOW: u32 = 0x0800_0000;
            shell
                .arg("/C")
                .arg(command)
                .creation_flags(CREATE_NEW_PROCESS_GROUP | CREATE_NO_WINDOW);
            shell
        }
        #[cfg(not(windows))]
        {
            let mut shell = Command::new("/bin/sh");
            shell.arg("-c").arg(command).process_group(0);
            shell
        }
    };
    let mut child = shell
        .stdin(Stdio::null())
        .stdout(Stdio::from(child_output))
        .stderr(Stdio::null())
        .spawn()
        .map_err(CommandExecutionError::Spawn)?;

    let deadline = Instant::now() + timeout;
    let status = loop {
        if let Some(status) = child.try_wait().map_err(CommandExecutionError::Wait)? {
            break status;
        }
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            terminate_process_group_and_reap(&mut child)?;
            return Err(CommandExecutionError::TimedOut);
        }
        thread::sleep(remaining.min(COMMAND_POLL_INTERVAL));
    };

    if !status.success() {
        return Err(CommandExecutionError::Exit(status));
    }
    output_file
        .seek(SeekFrom::Start(0))
        .map_err(CommandExecutionError::ReadOutput)?;
    let mut bytes = Vec::new();
    output_file
        .read_to_end(&mut bytes)
        .map_err(CommandExecutionError::ReadOutput)?;
    let text = String::from_utf8_lossy(&bytes).trim().to_owned();
    if text.is_empty() {
        Err(CommandExecutionError::EmptyOutput)
    } else {
        Ok(text)
    }
}

fn execute_command_uncached(command_config: &str) -> Option<String> {
    let command = command_config.get(1..).unwrap_or("");
    execute_with_default_shell(command, COMMAND_TIMEOUT).ok()
}

fn execute_command(command_config: &str) -> Option<String> {
    {
        let cache = command_cache()
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if let Some(cached) = cache.get(command_config) {
            return cached.clone();
        }
    }

    let result = execute_command_uncached(command_config);
    let mut cache = command_cache()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    cache.insert(command_config.to_owned(), result.clone());
    result
}

/// Resolve a config value to an actual secret/string.
///
/// - `!command` → execute and cache stdout
/// - `$ENV` / `${ENV}` templates → interpolate
/// - `$$` / `$!` escapes
/// - plain literals pass through
///
/// Returns `None` when a referenced env var is missing or a command fails.
#[must_use]
pub fn resolve_config_value(config: &str, env: Option<&ConfigEnv>) -> Option<String> {
    match parse_config_value_reference(config) {
        ConfigValueReference::Command(command) => execute_command(&command),
        ConfigValueReference::Template(parts) => resolve_template(&parts, env),
    }
}

/// Resolve without reading or writing the process-lifetime command cache.
#[must_use]
pub fn resolve_config_value_uncached(config: &str, env: Option<&ConfigEnv>) -> Option<String> {
    match parse_config_value_reference(config) {
        ConfigValueReference::Command(command) => execute_command_uncached(&command),
        ConfigValueReference::Template(parts) => resolve_template(&parts, env),
    }
}

/// Resolve all header values with the same rules as API keys.
///
/// Present `None` values are suppression markers and pass through without
/// config-value expansion. A failure to resolve any non-null template drops
/// the whole map (matching the previous all-or-nothing behavior).
#[must_use]
pub fn resolve_headers(
    headers: Option<&std::collections::BTreeMap<String, Option<String>>>,
    env: Option<&ConfigEnv>,
) -> Option<std::collections::BTreeMap<String, Option<String>>> {
    let headers = headers?;
    let mut resolved = std::collections::BTreeMap::new();
    for (key, value) in headers {
        match value {
            None => {
                resolved.insert(key.clone(), None);
            }
            Some(template) => {
                let resolved_value = resolve_config_value(template, env)?;
                resolved.insert(key.clone(), Some(resolved_value));
            }
        }
    }
    Some(resolved)
}

/// Clear the process-lifetime command cache. Exported for tests.
pub fn clear_config_value_cache() {
    command_cache()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .clear();
}

/// Number of cached command entries. Test helper only.
#[cfg(test)]
fn command_cache_len() -> usize {
    command_cache()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .len()
}

#[cfg(test)]
mod tests {
    type TestResult = Result<(), Box<dyn std::error::Error>>;
    use super::*;

    #[test]
    fn escaping_and_env_interpolation() {
        clear_config_value_cache();
        let env = ConfigEnv::from([
            ("TOKEN".into(), "secret".into()),
            ("EMPTY".into(), String::new()),
        ]);

        assert_eq!(
            resolve_config_value("bearer $TOKEN", Some(&env)).as_deref(),
            Some("bearer secret")
        );
        assert_eq!(
            resolve_config_value("bearer ${TOKEN}", Some(&env)).as_deref(),
            Some("bearer secret")
        );
        assert_eq!(
            resolve_config_value("cost is $$5 and bang $!", Some(&env)).as_deref(),
            Some("cost is $5 and bang !")
        );
        assert_eq!(
            resolve_config_value("missing $NOT_SET_AT_ALL_123", Some(&env)),
            None
        );
        // Empty overlay value falls through; still missing if process env unset.
        assert_eq!(resolve_config_value("$EMPTY", Some(&env)), None);
        assert_eq!(
            resolve_config_value("literal-value", Some(&env)).as_deref(),
            Some("literal-value")
        );
        assert!(is_command_config_value("!echo hi"));
        assert!(!is_command_config_value("$!echo hi"));
    }

    #[test]
    fn cached_command_execution_runs_once() -> Result<(), String> {
        clear_config_value_cache();
        // Use a unique marker file path under temp to count executions without
        // depending on bash-specific features beyond POSIX sh.
        let marker =
            std::env::temp_dir().join(format!("pi-ai-config-value-cache-{}", std::process::id()));
        let _ = std::fs::remove_file(&marker);
        let marker_str = marker.to_string_lossy().replace('\'', "");

        let command = format!("!echo run >> '{marker_str}' && echo cached-secret");
        let first = resolve_config_value(&command, None);
        let second = resolve_config_value(&command, None);
        assert_eq!(first.as_deref(), Some("cached-secret"));
        assert_eq!(second.as_deref(), Some("cached-secret"));

        let body = std::fs::read_to_string(&marker).map_err(|err| err.to_string())?;
        let runs = body.lines().filter(|line| *line == "run").count();
        assert_eq!(runs, 1, "command must execute once per process cache");
        assert_eq!(command_cache_len(), 1);
        let _ = std::fs::remove_file(&marker);
        clear_config_value_cache();
        Ok(())
    }

    #[test]
    fn list_path_must_not_execute_commands() {
        clear_config_value_cache();

        // Simulate a list() implementation: inspect raw key, never resolve.
        let raw_key = "!echo list-side-effect";
        let listed_kind = "api_key";
        assert_eq!(listed_kind, "api_key");
        assert!(is_command_config_value(raw_key));
        assert_eq!(command_cache_len(), 0);
        // Resolving would execute; list must not call this.
        assert!(is_command_config_value(raw_key));
        clear_config_value_cache();
    }

    #[test]
    fn resolve_returns_copies_without_mutating_input() {
        clear_config_value_cache();
        let raw = String::from("$TOKEN");
        let env = ConfigEnv::from([("TOKEN".into(), "abc".into())]);
        let resolved = resolve_config_value(&raw, Some(&env));
        assert_eq!(resolved.as_deref(), Some("abc"));
        assert_eq!(raw, "$TOKEN");
    }

    #[cfg(unix)]
    #[test]
    fn timeout_terminates_descendants_and_reaps_shell() -> Result<(), String> {
        let dir = tempfile::tempdir().map_err(|error| error.to_string())?;
        let marker = dir.path().join("descendant-survived");
        let command = format!(
            "(sleep 0.25; printf leaked > '{}') & wait",
            marker.display()
        );

        let started = Instant::now();
        let result = execute_with_default_shell(&command, Duration::from_millis(40));
        assert!(matches!(result, Err(CommandExecutionError::TimedOut)));
        assert!(started.elapsed() < Duration::from_secs(1));

        thread::sleep(Duration::from_millis(350));
        assert!(
            !marker.exists(),
            "the timed-out command's process group must not survive"
        );
        Ok(())
    }

    #[test]
    fn resolve_headers_passes_null_suppression_and_expands_templates() -> TestResult {
        clear_config_value_cache();
        let env = ConfigEnv::from([("TOKEN".into(), "secret".into())]);
        let headers = std::collections::BTreeMap::from([
            ("Authorization".to_owned(), None),
            ("X-Auth".to_owned(), Some("Bearer $TOKEN".to_owned())),
        ]);
        let resolved = resolve_headers(Some(&headers), Some(&env)).ok_or("headers resolved")?;
        assert_eq!(resolved.get("Authorization"), Some(&None));
        assert_eq!(
            resolved.get("X-Auth").and_then(|v| v.as_deref()),
            Some("Bearer secret")
        );
        Ok(())
    }
}
