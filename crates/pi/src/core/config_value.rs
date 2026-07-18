//! Resolve configuration values that may be shell commands, environment
//! variables, or literals.
//!
//! Port of `.references/pi/packages/coding-agent/src/core/resolve-config-value.ts`.
//!
//! Grammar:
//! - values starting with `!` execute the remainder as a shell command (stdout,
//!   trimmed) and cache the result for the process lifetime, including `None`
//! - `$NAME` / `${NAME}` interpolate environment values
//! - `$$` → literal `$`, `$!` → literal `!`
//! - unresolved environment references make the whole template resolve to
//!   `None`
//!
//! Command execution is synchronous with a 10s timeout, stderr discarded, and
//! stdout drained concurrently to avoid pipe deadlock. Unix uses
//! `/bin/sh -c` in its own process group; Windows uses `cmd /C`. Timed-out
//! children are killed (process group on Unix) and waited on.

use std::collections::HashMap;
use std::io::Read;
use std::process::{Command, Stdio};
use std::sync::{LazyLock, Mutex};
use std::thread;
use std::time::Duration;

use wait_timeout::ChildExt;

/// Provider-scoped env overlay used during resolution.
///
/// Lookup order matches TypeScript `env?.[name] || process.env[name]`: a missing
/// or empty overlay value falls through to the process environment, and empty
/// process values are treated as unset.
pub type ConfigEnv = HashMap<String, String>;

/// Default command execution timeout matching the TypeScript `timeout: 10000`.
const COMMAND_TIMEOUT: Duration = Duration::from_secs(10);

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
type CommandCacheGuard = std::sync::MutexGuard<'static, HashMap<String, Option<String>>>;

fn lock_command_cache() -> CommandCacheGuard {
    match command_cache().lock() {
        Ok(cache) => cache,
        Err(poisoned) => poisoned.into_inner(),
    }
}

fn command_body(config: &str) -> &str {
    config.strip_prefix('!').map_or("", std::convert::identity)
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

fn lookup_process_env(name: &str) -> Option<String> {
    #[cfg(test)]
    match test_env_lookup(name) {
        TestOverride::Present(value) => return value,
        TestOverride::Absent => {}
    }
    match std::env::var(name) {
        Ok(value) if !value.is_empty() => Some(value),
        _ => None,
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
    lookup_process_env(name)
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

/// Return all unique env-var names referenced by a template config value, in
/// first-seen order.
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

fn command_timeout() -> Duration {
    #[cfg(test)]
    if let Some(timeout) = test_command_timeout() {
        return timeout;
    }
    COMMAND_TIMEOUT
}

/// Kill a timed-out child. On Unix the shell is started in its own process
/// group so this terminates descendants as well.
fn kill_timed_out_child(child: &mut std::process::Child) {
    #[cfg(unix)]
    {
        use nix::sys::signal::{Signal, killpg};
        use nix::unistd::Pid;

        let pid = child.id();
        // The shell was launched with `process_group(0)`, so its PID is the
        // process-group leader. killpg SIGKILL the whole tree; if that fails
        // (already reaped / race), fall back to killing the shell alone.
        if killpg(Pid::from_raw(pid.cast_signed()), Signal::SIGKILL).is_err() {
            let _ = child.kill();
        }
    }
    #[cfg(not(unix))]
    {
        let _ = child.kill();
    }
    let _ = child.wait();
}

/// Execute `command` with the platform default shell.
///
/// Stderr is discarded. Stdout is drained on a helper thread so a large
/// producer cannot deadlock against a blocked reader while we wait for exit.
fn execute_with_default_shell(command: &str) -> Option<String> {
    #[cfg(test)]
    match test_command_runner(command) {
        TestOverride::Present(result) => return result,
        TestOverride::Absent => {}
    }

    let mut child = {
        #[cfg(windows)]
        {
            Command::new("cmd")
                .arg("/C")
                .arg(command)
                .stdin(Stdio::null())
                .stdout(Stdio::piped())
                .stderr(Stdio::null())
                .spawn()
        }
        #[cfg(not(windows))]
        {
            use std::os::unix::process::CommandExt;
            Command::new("/bin/sh")
                .arg("-c")
                .arg(command)
                .stdin(Stdio::null())
                .stdout(Stdio::piped())
                .stderr(Stdio::null())
                // Own process group so timeout can kill the whole tree.
                .process_group(0)
                .spawn()
        }
    }
    .ok()?;

    let stdout = child.stdout.take();
    let (tx, rx) = std::sync::mpsc::channel();
    let drain = thread::spawn(move || {
        let mut buf = Vec::new();
        if let Some(mut pipe) = stdout {
            let _ = pipe.read_to_end(&mut buf);
        }
        let _ = tx.send(buf);
    });

    let timeout = command_timeout();
    let started = std::time::Instant::now();

    let Ok(Some(status)) = child.wait_timeout(timeout) else {
        kill_timed_out_child(&mut child);
        // Bounded drain wait after kill so a stuck pipe cannot hang forever.
        let remaining = timeout.saturating_sub(started.elapsed());
        let _ = rx.recv_timeout(remaining.max(Duration::from_millis(50)));
        // Detach the drain thread; pipe close after process-group kill is
        // best-effort and must not block the caller.
        drop(drain);
        return None;
    };

    // Shell exited, but a detached descendant may still hold the pipe. Bound
    // the drain wait by the overall deadline, then kill the process group.
    let remaining = timeout.saturating_sub(started.elapsed());
    let Ok(bytes) = rx.recv_timeout(remaining) else {
        kill_timed_out_child(&mut child);
        drop(drain);
        return None;
    };
    if drain.join().is_err() {
        return None;
    }

    if !status.success() {
        return None;
    }
    let text = String::from_utf8_lossy(&bytes).trim().to_owned();
    if text.is_empty() { None } else { Some(text) }
}

fn execute_command_uncached(command_config: &str) -> Option<String> {
    execute_with_default_shell(command_body(command_config))
}

fn execute_command(command_config: &str) -> Option<String> {
    // Hold the cache lock across the whole miss path so concurrent callers of
    // the same (or any) command do not double-execute. Config-value commands
    // are rare auth/header resolutions; serializing them is the correct
    // process-lifetime cache semantics.
    let mut cache = lock_command_cache();

    if let Some(cached) = cache.get(command_config) {
        return cached.clone();
    }

    let result = execute_command_uncached(command_config);
    // Cache successes and failures (`None`) for the process lifetime.
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

/// Resolve a required config value or return the exact TypeScript error string.
///
/// Uses the uncached path, matching `resolveConfigValueOrThrow`.
///
/// # Errors
///
/// Returns an error message when a `!command` fails or when one or more
/// referenced environment variables are unset.
pub fn resolve_config_value_or_throw(
    config: &str,
    description: &str,
    env: Option<&ConfigEnv>,
) -> Result<String, String> {
    if let Some(value) = resolve_config_value_uncached(config, env) {
        return Ok(value);
    }

    match parse_config_value_reference(config) {
        ConfigValueReference::Command(command) => Err(format!(
            "Failed to resolve {description} from shell command: {}",
            command_body(&command)
        )),

        ConfigValueReference::Template(_) => {
            let missing = get_missing_config_value_env_var_names(config, env);
            match missing.as_slice() {
                [name] => Err(format!(
                    "Failed to resolve {description} from environment variable: {name}"
                )),
                names if names.len() > 1 => Err(format!(
                    "Failed to resolve {description} from environment variables: {}",
                    names.join(", ")
                )),
                _ => Err(format!("Failed to resolve {description}")),
            }
        }
    }
}

/// Resolve all header values with the same rules as API keys.
///
/// Falsy resolved values (including empty strings) are dropped. Returns
/// `None` when the input is `None` or every value resolves away.
#[must_use]
pub fn resolve_headers<S: std::hash::BuildHasher>(
    headers: Option<&HashMap<String, String, S>>,
    env: Option<&ConfigEnv>,
) -> Option<HashMap<String, String>> {
    let headers = headers?;
    let mut resolved = HashMap::new();
    for (key, value) in headers {
        if let Some(resolved_value) = resolve_config_value(value, env)
            && !resolved_value.is_empty()
        {
            resolved.insert(key.clone(), resolved_value);
        }
    }
    if resolved.is_empty() {
        None
    } else {
        Some(resolved)
    }
}

/// Resolve all header values, failing on the first unresolvable entry.
///
/// # Errors
///
/// Propagates [`resolve_config_value_or_throw`] errors with
/// `` `{description} header "{key}"` `` as the description.
pub fn resolve_headers_or_throw<S: std::hash::BuildHasher>(
    headers: Option<&HashMap<String, String, S>>,
    description: &str,
    env: Option<&ConfigEnv>,
) -> Result<Option<HashMap<String, String>>, String> {
    let Some(headers) = headers else {
        return Ok(None);
    };
    let mut resolved = HashMap::new();
    for (key, value) in headers {
        let resolved_value =
            resolve_config_value_or_throw(value, &format!("{description} header \"{key}\""), env)?;
        resolved.insert(key.clone(), resolved_value);
    }
    if resolved.is_empty() {
        Ok(None)
    } else {
        Ok(Some(resolved))
    }
}

/// Clear the process-lifetime command cache. Exported for tests.
pub fn clear_config_value_cache() {
    lock_command_cache().clear();
}

// ---------------------------------------------------------------------------
// Test-only injection seams (not a general abstraction).
// ---------------------------------------------------------------------------

#[cfg(test)]
#[derive(Clone, Debug)]
enum TestOverride<T> {
    Absent,
    Present(T),
}

#[cfg(test)]
type TestCommandRunner = fn(&str) -> Option<String>;

#[cfg(test)]
thread_local! {
    static TEST_COMMAND_RUNNER: std::cell::Cell<Option<TestCommandRunner>> =
        const { std::cell::Cell::new(None) };

    static TEST_COMMAND_TIMEOUT: std::cell::Cell<Option<Duration>> =
        const { std::cell::Cell::new(None) };
    static TEST_ENV: std::cell::RefCell<Option<HashMap<String, String>>> =
        const { std::cell::RefCell::new(None) };
}

#[cfg(test)]
fn test_command_runner(command: &str) -> TestOverride<Option<String>> {
    TEST_COMMAND_RUNNER.with(|cell| match cell.get() {
        Some(runner) => TestOverride::Present(runner(command)),
        None => TestOverride::Absent,
    })
}

#[cfg(test)]
fn test_command_timeout() -> Option<Duration> {
    TEST_COMMAND_TIMEOUT.with(std::cell::Cell::get)
}

#[cfg(test)]
fn test_env_lookup(name: &str) -> TestOverride<Option<String>> {
    TEST_ENV.with(|cell| {
        let guard = cell.borrow();
        match guard.as_ref() {
            Some(map) => {
                TestOverride::Present(map.get(name).filter(|value| !value.is_empty()).cloned())
            }
            None => TestOverride::Absent,
        }
    })
}

#[cfg(test)]
fn with_test_command_runner<R>(runner: TestCommandRunner, f: impl FnOnce() -> R) -> R {
    struct Restore(Option<TestCommandRunner>);

    impl Drop for Restore {
        fn drop(&mut self) {
            TEST_COMMAND_RUNNER.with(|cell| cell.set(self.0.take()));
        }
    }
    TEST_COMMAND_RUNNER.with(|cell| {
        let previous = cell.replace(Some(runner));
        let _restore = Restore(previous);
        f()
    })
}

#[cfg(test)]
fn with_test_command_timeout<R>(timeout: Duration, f: impl FnOnce() -> R) -> R {
    struct Restore(Option<Duration>);
    impl Drop for Restore {
        fn drop(&mut self) {
            TEST_COMMAND_TIMEOUT.with(|cell| cell.set(self.0.take()));
        }
    }
    TEST_COMMAND_TIMEOUT.with(|cell| {
        let previous = cell.replace(Some(timeout));
        let _restore = Restore(previous);
        f()
    })
}

#[cfg(test)]
fn with_test_process_env<R>(env: HashMap<String, String>, f: impl FnOnce() -> R) -> R {
    struct Restore(Option<HashMap<String, String>>);
    impl Drop for Restore {
        fn drop(&mut self) {
            TEST_ENV.with(|cell| {
                *cell.borrow_mut() = self.0.take();
            });
        }
    }
    TEST_ENV.with(|cell| {
        let previous = cell.replace(Some(env));
        let _restore = Restore(previous);
        f()
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::{LazyLock, Mutex, MutexGuard};
    use std::time::Instant;

    /// Process-global cache + command runner state is shared; serialize tests
    /// that clear/inspect it so parallel cargo test workers do not race.
    fn cache_test_lock() -> MutexGuard<'static, ()> {
        static LOCK: LazyLock<Mutex<()>> = LazyLock::new(|| Mutex::new(()));
        match LOCK.lock() {
            Ok(guard) => guard,
            Err(poisoned) => poisoned.into_inner(),
        }
    }

    fn map(pairs: &[(&str, &str)]) -> ConfigEnv {
        pairs
            .iter()
            .map(|(k, v)| ((*k).to_owned(), (*v).to_owned()))
            .collect()
    }
    static CACHED_RUNNER_HITS: AtomicUsize = AtomicUsize::new(0);
    static UNCACHED_RUNNER_HITS: AtomicUsize = AtomicUsize::new(0);

    fn cached_counting_runner(command: &str) -> Option<String> {
        CACHED_RUNNER_HITS.fetch_add(1, Ordering::SeqCst);
        if command.contains("fail") {
            None
        } else {
            Some("value".to_owned())
        }
    }

    fn uncached_counting_runner(command: &str) -> Option<String> {
        UNCACHED_RUNNER_HITS.fetch_add(1, Ordering::SeqCst);
        (!command.is_empty()).then(|| "value".to_owned())
    }

    #[test]
    fn resolves_literals_environment_templates_and_escapes() {
        let _guard = cache_test_lock();
        clear_config_value_cache();
        let process = map(&[("TEST_CONFIG_LEFT", "left"), ("TEST_CONFIG_RIGHT", "right")]);
        with_test_process_env(process, || {
            assert_eq!(
                resolve_config_value("literal-key", None).as_deref(),
                Some("literal-key")
            );
            assert_eq!(
                resolve_config_value("$TEST_CONFIG_LEFT", None).as_deref(),
                Some("left")
            );
            assert_eq!(
                resolve_config_value("${TEST_CONFIG_LEFT}_$TEST_CONFIG_RIGHT", None).as_deref(),
                Some("left_right")
            );
            assert_eq!(
                resolve_config_value("$$TEST_CONFIG_LEFT", None).as_deref(),
                Some("$TEST_CONFIG_LEFT")
            );
            assert_eq!(
                resolve_config_value("$!literal-$TEST_CONFIG_RIGHT", None).as_deref(),
                Some("!literal-right")
            );
        });
    }

    #[test]
    fn uses_credential_scoped_environment_before_process_env() {
        let _guard = cache_test_lock();
        clear_config_value_cache();
        let process = map(&[("TEST_CONFIG_SCOPED", "process")]);
        let overlay = map(&[("TEST_CONFIG_SCOPED", "credential")]);
        with_test_process_env(process, || {
            assert_eq!(
                resolve_config_value("$TEST_CONFIG_SCOPED", Some(&overlay)).as_deref(),
                Some("credential")
            );
        });
    }

    #[test]
    fn empty_overlay_falls_back_to_process_env() {
        let _guard = cache_test_lock();
        clear_config_value_cache();
        let process = map(&[("TEST_CONFIG_EMPTY_OVERLAY", "from-process")]);
        let overlay = map(&[("TEST_CONFIG_EMPTY_OVERLAY", "")]);
        with_test_process_env(process, || {
            assert_eq!(
                resolve_config_value("$TEST_CONFIG_EMPTY_OVERLAY", Some(&overlay)).as_deref(),
                Some("from-process")
            );
        });
    }

    #[test]
    fn missing_env_part_fails_whole_template() {
        let _guard = cache_test_lock();
        clear_config_value_cache();
        let process = map(&[("PRESENT", "yes")]);
        with_test_process_env(process, || {
            assert_eq!(resolve_config_value("$PRESENT-$MISSING_XYZ", None), None);
            assert_eq!(
                // `$A_${B}`: unbraced prefix greedily takes trailing `_`, so
                // names are `A_`, `B`, then the second `$A`.
                get_config_value_env_var_names("$A_${B}_$A"),
                vec!["A_".to_owned(), "B".to_owned(), "A".to_owned()]
            );
            assert_eq!(
                get_config_value_env_var_names("${A}_${B}_${A}"),
                vec!["A".to_owned(), "B".to_owned()]
            );
            assert_eq!(
                get_config_value_env_var_name("$ONLY"),
                Some("ONLY".to_owned())
            );
            assert_eq!(
                get_config_value_env_var_name("${ONLY}"),
                Some("ONLY".to_owned())
            );
            assert_eq!(get_config_value_env_var_name("$A$B"), None);
            assert_eq!(get_config_value_env_var_name("!echo hi"), None);
            assert!(is_command_config_value("!echo hi"));
            assert!(!is_command_config_value("$!echo hi"));
            assert!(is_config_value_configured("literal", None));
            assert!(!is_config_value_configured("$MISSING_XYZ", None));
        });
    }

    #[test]
    fn invalid_braced_names_stay_literal() {
        let _guard = cache_test_lock();
        clear_config_value_cache();
        assert_eq!(
            resolve_config_value("${1BAD}", None).as_deref(),
            Some("${1BAD}")
        );
        assert_eq!(
            resolve_config_value("${has-dash}", None).as_deref(),
            Some("${has-dash}")
        );
        // Unclosed brace becomes a literal `$` and continues parsing.
        assert_eq!(
            resolve_config_value("${unclosed", None).as_deref(),
            Some("${unclosed")
        );
        assert_eq!(resolve_config_value("$", None).as_deref(), Some("$"));
        assert_eq!(resolve_config_value("pre$", None).as_deref(), Some("pre$"));
    }

    #[test]
    fn executes_shell_commands_and_trims_output() {
        let _guard = cache_test_lock();
        clear_config_value_cache();
        assert_eq!(
            resolve_config_value("!echo '  spaced-key  '", None).as_deref(),
            Some("spaced-key")
        );
        assert_eq!(
            resolve_config_value("!printf 'line1\\nline2'", None).as_deref(),
            Some("line1\nline2")
        );
        assert_eq!(
            resolve_config_value("!echo 'hello world' | tr ' ' '-'", None).as_deref(),
            Some("hello-world")
        );
    }

    #[test]
    fn returns_none_on_command_failure_or_empty_output() {
        let _guard = cache_test_lock();
        clear_config_value_cache();
        assert_eq!(resolve_config_value("!exit 1", None), None);
        assert_eq!(
            resolve_config_value("!nonexistent-command-12345", None),
            None
        );
        assert_eq!(resolve_config_value("!printf ''", None), None);
    }

    #[test]
    fn caches_successful_and_failed_commands_until_cleared() {
        let _guard = cache_test_lock();
        clear_config_value_cache();
        CACHED_RUNNER_HITS.store(0, Ordering::SeqCst);

        with_test_command_runner(cached_counting_runner, || {
            let success = "!success-command";
            assert_eq!(
                resolve_config_value(success, None).as_deref(),
                Some("value")
            );
            assert_eq!(
                resolve_config_value(success, None).as_deref(),
                Some("value")
            );
            assert_eq!(CACHED_RUNNER_HITS.load(Ordering::SeqCst), 1);

            clear_config_value_cache();
            assert_eq!(
                resolve_config_value(success, None).as_deref(),
                Some("value")
            );
            assert_eq!(CACHED_RUNNER_HITS.load(Ordering::SeqCst), 2);

            let failure = "!fail-command";
            assert_eq!(resolve_config_value(failure, None), None);
            assert_eq!(resolve_config_value(failure, None), None);
            assert_eq!(CACHED_RUNNER_HITS.load(Ordering::SeqCst), 3);
        });
    }

    #[test]
    fn does_not_cache_environment_values() {
        let _guard = cache_test_lock();
        clear_config_value_cache();
        with_test_process_env(map(&[("TEST_CONFIG_DYNAMIC", "first")]), || {
            assert_eq!(
                resolve_config_value("$TEST_CONFIG_DYNAMIC", None).as_deref(),
                Some("first")
            );
        });
        with_test_process_env(map(&[("TEST_CONFIG_DYNAMIC", "second")]), || {
            assert_eq!(
                resolve_config_value("$TEST_CONFIG_DYNAMIC", None).as_deref(),
                Some("second")
            );
        });
    }

    #[test]
    fn uncached_resolution_executes_every_call() {
        let _guard = cache_test_lock();
        clear_config_value_cache();
        UNCACHED_RUNNER_HITS.store(0, Ordering::SeqCst);
        with_test_command_runner(uncached_counting_runner, || {
            let command = "!uncached-counter";
            assert_eq!(
                resolve_config_value_uncached(command, None).as_deref(),
                Some("value")
            );
            assert_eq!(
                resolve_config_value_uncached(command, None).as_deref(),
                Some("value")
            );
            assert_eq!(UNCACHED_RUNNER_HITS.load(Ordering::SeqCst), 2);
        });
    }

    #[cfg(unix)]
    #[test]
    fn large_stdout_is_drained_without_deadlock() -> Result<(), String> {
        let _guard = cache_test_lock();
        clear_config_value_cache();
        // ~256 KiB of output exercises concurrent drain vs wait.
        // Prefer POSIX tools only (no python3 dependency).
        let command = "!dd if=/dev/zero bs=1024 count=256 2>/dev/null | tr '\\0' 'a'";
        let value = resolve_config_value_uncached(command, None)
            .ok_or_else(|| "large stdout should resolve".to_owned())?;
        if value.len() != 256 * 1024 {
            return Err(format!(
                "expected {} bytes, got {}",
                256 * 1024,
                value.len()
            ));
        }
        if !value.bytes().all(|b| b == b'a') {
            return Err("expected all 'a' bytes".to_owned());
        }
        Ok(())
    }

    #[cfg(unix)]
    #[test]
    fn timeout_kills_child_without_leak() -> Result<(), String> {
        let _guard = cache_test_lock();
        clear_config_value_cache();
        let marker =
            std::env::temp_dir().join(format!("pi-config-value-timeout-{}", std::process::id()));
        let _ = std::fs::remove_file(&marker);
        let marker_str = marker.to_string_lossy().replace('\'', r"'\''");
        // Write a pid file, sleep long enough to hit the shortened timeout, and
        // only create a "survived" marker if the sleep finishes.
        let command =
            format!("echo $$ > '{marker_str}'; sleep 30; echo survived >> '{marker_str}'");
        let full = format!("!{command}");

        let started = Instant::now();
        let result = with_test_command_timeout(Duration::from_millis(200), || {
            resolve_config_value_uncached(&full, None)
        });
        if result.is_some() {
            return Err("timed-out command unexpectedly resolved".to_owned());
        }
        if started.elapsed() >= Duration::from_secs(5) {
            return Err("timeout path did not return promptly".to_owned());
        }

        // Give the kernel a moment to reap; then assert the child is gone.
        thread::sleep(Duration::from_millis(150));
        let pid_text = std::fs::read_to_string(&marker)
            .map_err(|error| format!("failed reading timeout marker: {error}"))?;
        let pid_line = pid_text
            .lines()
            .next()
            .ok_or_else(|| "timeout marker had no pid".to_owned())?
            .trim();
        if pid_text.contains("survived") {
            return Err("child finished after kill".to_owned());
        }
        let pid = pid_line
            .parse::<i32>()
            .map_err(|error| format!("invalid child pid: {error}"))?;
        let gone = matches!(
            nix::sys::signal::kill(nix::unistd::Pid::from_raw(pid), None),
            Err(nix::errno::Errno::ESRCH)
        );
        let _ = std::fs::remove_file(&marker);
        if gone {
            Ok(())
        } else {
            Err(format!("timed-out child pid {pid} leaked"))
        }
    }

    #[test]
    fn or_throw_uses_exact_error_strings() -> Result<(), String> {
        let _guard = cache_test_lock();
        clear_config_value_cache();
        with_test_process_env(HashMap::new(), || {
            match resolve_config_value_or_throw("!false", "API key", None) {
                Ok(value) => {
                    return Err(format!("command failure expected, got {value}"));
                }
                Err(err) => {
                    if err != "Failed to resolve API key from shell command: false" {
                        return Err(format!("unexpected command error: {err}"));
                    }
                }
            }

            match resolve_config_value_or_throw("$MISSING_ONE", "API key", None) {
                Ok(value) => {
                    return Err(format!("one missing env expected, got {value}"));
                }
                Err(err) => {
                    if err != "Failed to resolve API key from environment variable: MISSING_ONE" {
                        return Err(format!("unexpected one-env error: {err}"));
                    }
                }
            }

            match resolve_config_value_or_throw("$MISSING_A-$MISSING_B", "API key", None) {
                Ok(value) => {
                    return Err(format!("two missing env expected, got {value}"));
                }
                Err(err) => {
                    if err
                        != "Failed to resolve API key from environment variables: MISSING_A, MISSING_B"
                    {
                        return Err(format!("unexpected multi-env error: {err}"));
                    }
                }
            }
            Ok(())
        })
    }

    #[test]
    fn resolve_headers_drops_falsy_and_or_throw_describes_key() -> Result<(), String> {
        let _guard = cache_test_lock();
        clear_config_value_cache();
        let process = map(&[("HDR", "value")]);
        with_test_process_env(process, || {
            let headers = map(&[
                ("Authorization", "Bearer $HDR"),
                ("Empty", "!printf ''"),
                ("Missing", "$NOPE"),
            ]);
            let resolved = resolve_headers(Some(&headers), None)
                .ok_or_else(|| "some headers should resolve".to_owned())?;
            if resolved.get("Authorization").map(String::as_str) != Some("Bearer value") {
                return Err(format!(
                    "Authorization mismatch: {:?}",
                    resolved.get("Authorization")
                ));
            }
            if resolved.contains_key("Empty") {
                return Err("Empty header should be dropped".to_owned());
            }
            if resolved.contains_key("Missing") {
                return Err("Missing header should be dropped".to_owned());
            }

            match resolve_headers_or_throw(Some(&headers), "provider \"x\"", None) {
                Ok(value) => Err(format!("missing header env expected, got {value:?}")),
                Err(err) => {
                    if err.contains("Failed to resolve provider \"x\" header \"")
                        && (err.contains("Empty") || err.contains("Missing"))
                    {
                        Ok(())
                    } else {
                        Err(format!("unexpected error: {err}"))
                    }
                }
            }
        })
    }

    #[test]
    fn or_throw_succeeds_for_resolved_values() -> Result<(), String> {
        let _guard = cache_test_lock();
        clear_config_value_cache();
        let process = map(&[("OK", "token")]);
        with_test_process_env(process, || {
            let key = resolve_config_value_or_throw("$OK", "API key", None)?;
            if key != "token" {
                return Err(format!("expected token, got {key}"));
            }
            let cmd = resolve_config_value_or_throw("!echo ok", "API key", None)?;
            if cmd != "ok" {
                return Err(format!("expected ok, got {cmd}"));
            }
            let headers = map(&[("X", "$OK")]);
            let resolved = resolve_headers_or_throw(Some(&headers), "provider", None)?;
            let x = resolved
                .as_ref()
                .and_then(|m| m.get("X"))
                .map(String::as_str);
            if x != Some("token") {
                return Err(format!("expected header token, got {x:?}"));
            }
            if resolve_headers::<std::collections::hash_map::RandomState>(None, None).is_some() {
                return Err("None headers should stay None".to_owned());
            }
            match resolve_headers_or_throw::<std::collections::hash_map::RandomState>(
                None, "provider", None,
            )? {
                None => Ok(()),
                Some(map) => Err(format!("expected None, got {map:?}")),
            }
        })
    }
}
