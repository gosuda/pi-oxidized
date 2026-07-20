//! First-run setup gating and persistence.
//!
//! Ports `cli/startup-ui.ts` `shouldRunFirstTimeSetup` plus the persistence
//! half of `showFirstTimeSetup` from the TypeScript coding-agent. The
//! interactive two-step dialog (theme, then analytics opt-in) is TUI-owned and
//! intentionally not ported here; this module owns only the gate and the
//! settings write.
//!
//! The gate runs only when **all** are true:
//! 1. Official distribution (`@earendil-works/pi-coding-agent` / `pi` / `.pi`).
//! 2. Experimental features enabled (`PI_EXPERIMENTAL == "1"`).
//! 3. No agent-dir override (`PI_CODING_AGENT_DIR` unset).
//! 4. No `settings.json` yet (first run).
//!
//! Persisting any selection writes `settings.json`, which suppresses future
//! first-run prompts by failing gate 4.

use crate::core::config::{
    ENV_AGENT_DIR, get_agent_dir_with, get_settings_path_with, is_official_distribution,
};
use crate::core::experimental::are_experimental_features_enabled;
use crate::core::settings::{SettingsManager, ThemeMode};

/// A completed first-run selection ready to persist.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct FirstRunSelection {
    /// Theme name to store under the `theme` setting (family dark member).
    pub theme: String,
    /// Theme polarity mode to store under `themeMode`.
    pub theme_mode: ThemeMode,
    /// Whether the user opted into anonymous usage analytics.
    pub share_analytics: bool,
}

/// Pure gate predicate: whether first-run setup should run.
///
/// Takes each condition as an explicit argument so the decision is unit-testable
/// on any host without touching the process environment or filesystem.
#[must_use]
pub fn should_run_first_time_setup(
    official_distribution: bool,
    experimental_enabled: bool,
    agent_dir_override: Option<&str>,
    settings_exists: bool,
) -> bool {
    official_distribution
        && experimental_enabled
        && agent_dir_override.is_none()
        && !settings_exists
}

/// Host gate: resolve every condition from the process environment and disk.
///
/// `home_dir` and `settings_exists_override` are seams for tests; production
/// passes [`None`] for both so home comes from `dirs` and existence is probed
/// on disk.
#[must_use]
pub fn should_run_first_time_setup_on_host(
    home_dir: Option<&std::path::Path>,
    settings_exists_override: Option<bool>,
) -> bool {
    let agent_dir_env = std::env::var(ENV_AGENT_DIR).ok();
    let settings_exists = settings_exists_override.unwrap_or_else(|| {
        let agent_dir = get_agent_dir_with(agent_dir_env.as_deref(), home_dir);
        get_settings_path_with(&agent_dir).exists()
    });
    should_run_first_time_setup(
        is_official_distribution(),
        are_experimental_features_enabled(),
        agent_dir_env.as_deref(),
        settings_exists,
    )
}

/// Persist a first-run selection into global settings.
///
/// Writes `theme`, `themeMode`, and `enableAnalytics` (generating a tracking
/// id on first opt-in via [`SettingsManager::set_enable_analytics`]) and
/// flushes so the file lands on disk immediately, closing the first-run gate.
///
/// # Errors
///
/// Returns the underlying settings persistence error.
pub fn persist_first_run_selection(
    settings: &mut SettingsManager,
    selection: &FirstRunSelection,
) -> Result<(), String> {
    settings.set_theme(&selection.theme);
    settings.set_theme_mode(selection.theme_mode);
    settings.set_enable_analytics(selection.share_analytics);
    settings.flush();
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    type TestResult = Result<(), Box<dyn std::error::Error>>;

    #[test]
    fn gate_requires_all_conditions() {
        assert!(should_run_first_time_setup(true, true, None, false));

        // Any single failing condition disables the gate.
        assert!(!should_run_first_time_setup(false, true, None, false));
        assert!(!should_run_first_time_setup(true, false, None, false));
        assert!(!should_run_first_time_setup(
            true,
            true,
            Some("/tmp/x"),
            false
        ));
        assert!(!should_run_first_time_setup(true, true, None, true));
        assert!(!should_run_first_time_setup(true, true, Some(""), false));
    }

    #[test]
    fn empty_agent_dir_override_still_counts_as_unset() {
        // An empty PI_CODING_AGENT_DIR is treated as an override (Some("")),
        // matching Option::is_none semantics used by the reference's existence
        // check.
        assert!(!should_run_first_time_setup(true, true, Some(""), false));
    }

    #[test]
    fn persist_writes_theme_mode_and_analytics() -> TestResult {
        let dir = tempfile::tempdir()?;
        let agent = dir.path().join("agent");
        let project = dir.path().join("project");
        std::fs::create_dir_all(&agent)?;
        std::fs::create_dir_all(&project)?;
        let mut manager = SettingsManager::create(
            &project,
            Some(&agent),
            crate::core::settings::SettingsManagerCreateOptions::default(),
        );
        let selection = FirstRunSelection {
            theme: "motion-dark".to_owned(),
            theme_mode: ThemeMode::Auto,
            share_analytics: false,
        };
        persist_first_run_selection(&mut manager, &selection)?;
        assert_eq!(manager.get_theme().as_deref(), Some("motion-dark"));
        assert_eq!(manager.get_theme_mode(), ThemeMode::Auto);
        assert!(!manager.get_enable_analytics());
        Ok(())
    }
}
