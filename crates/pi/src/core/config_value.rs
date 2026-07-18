//! Product compatibility wrappers for canonical config-value resolution.
//!
//! Parsing, command execution, and the process-wide command cache are owned by
//! [`pi_ai::auth::config_value`]. This module preserves the historical
//! `pi::core::config_value` HashMap-shaped API without maintaining a second
//! parser or cache.

use std::collections::{BTreeMap, HashMap};

use pi_ai::auth::config_value as canonical;

/// Provider-scoped environment overlay accepted by the product API.
pub type ConfigEnv = HashMap<String, String>;

fn canonical_env(env: Option<&ConfigEnv>) -> Option<BTreeMap<String, String>> {
    env.map(|env| {
        env.iter()
            .map(|(key, value)| (key.clone(), value.clone()))
            .collect()
    })
}

/// Return the single env-var name when `config` is exactly `$NAME` / `${NAME}`.
#[must_use]
pub fn get_config_value_env_var_name(config: &str) -> Option<String> {
    canonical::get_config_value_env_var_name(config)
}

/// Return all unique env-var names referenced by a template config value.
#[must_use]
pub fn get_config_value_env_var_names(config: &str) -> Vec<String> {
    canonical::get_config_value_env_var_names(config)
}

/// Return referenced env-var names that are currently unset.
#[must_use]
pub fn get_missing_config_value_env_var_names(
    config: &str,
    env: Option<&ConfigEnv>,
) -> Vec<String> {
    let env = canonical_env(env);
    canonical::get_missing_config_value_env_var_names(config, env.as_ref())
}

/// Whether `config` is a `!command` value.
#[must_use]
pub fn is_command_config_value(config: &str) -> bool {
    canonical::is_command_config_value(config)
}

/// Whether every env var referenced by `config` is currently set.
#[must_use]
pub fn is_config_value_configured(config: &str, env: Option<&ConfigEnv>) -> bool {
    get_missing_config_value_env_var_names(config, env).is_empty()
}

/// Resolve a config value through the canonical process-wide cache.
#[must_use]
pub fn resolve_config_value(config: &str, env: Option<&ConfigEnv>) -> Option<String> {
    let env = canonical_env(env);
    canonical::resolve_config_value(config, env.as_ref())
}

/// Resolve a config value without reading or writing the command cache.
#[must_use]
pub fn resolve_config_value_uncached(config: &str, env: Option<&ConfigEnv>) -> Option<String> {
    let env = canonical_env(env);
    canonical::resolve_config_value_uncached(config, env.as_ref())
}

/// Resolve a required config value or return the historical product error text.
///
/// # Errors
///
/// Returns an error when a command fails or a referenced environment variable
/// is unset.
pub fn resolve_config_value_or_throw(
    config: &str,
    description: &str,
    env: Option<&ConfigEnv>,
) -> Result<String, String> {
    if let Some(value) = resolve_config_value_uncached(config, env) {
        return Ok(value);
    }

    if is_command_config_value(config) {
        return Err(format!(
            "Failed to resolve {description} from shell command: {}",
            config.strip_prefix('!').unwrap_or_default()
        ));
    }

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

/// Resolve all header values, dropping missing and empty values.
#[must_use]
pub fn resolve_headers<S: std::hash::BuildHasher>(
    headers: Option<&HashMap<String, String, S>>,
    env: Option<&ConfigEnv>,
) -> Option<HashMap<String, String>> {
    let headers = headers?;
    let mut resolved = HashMap::new();
    for (key, value) in headers {
        if let Some(value) = resolve_config_value(value, env)
            && !value.is_empty()
        {
            resolved.insert(key.clone(), value);
        }
    }
    (!resolved.is_empty()).then_some(resolved)
}

/// Resolve all header values, failing on the first unresolvable entry.
///
/// # Errors
///
/// Propagates [`resolve_config_value_or_throw`] with the header description.
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
        let value =
            resolve_config_value_or_throw(value, &format!("{description} header \"{key}\""), env)?;
        resolved.insert(key.clone(), value);
    }
    Ok((!resolved.is_empty()).then_some(resolved))
}

/// Clear the one canonical process-wide command cache.
pub fn clear_config_value_cache() {
    canonical::clear_config_value_cache();
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hashmap_compatibility_delegates_template_resolution() {
        let env = HashMap::from([
            ("TOKEN".to_owned(), "secret".to_owned()),
            ("REGION".to_owned(), "west".to_owned()),
        ]);
        assert_eq!(
            resolve_config_value("Bearer $TOKEN/${REGION}", Some(&env)).as_deref(),
            Some("Bearer secret/west")
        );
        assert_eq!(
            get_config_value_env_var_names("$TOKEN/${REGION}/$TOKEN"),
            ["TOKEN", "REGION"]
        );
    }

    #[test]
    fn compatibility_error_wording_is_preserved() {
        let env = HashMap::new();
        assert_eq!(
            resolve_config_value_or_throw("$MISSING", "API key", Some(&env)),
            Err("Failed to resolve API key from environment variable: MISSING".to_owned())
        );
    }
}
