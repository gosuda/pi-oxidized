//! Standalone resource-config TUI for `pi config`.
//!
//! Ports the observable surface of
//! `.references/pi/packages/coding-agent/src/cli/config-selector.ts` using the
//! shared pi-tui `SettingsList` + terminal guard loop (same machinery as the
//! interactive runtime, without a full agent session).

use std::io::{self, IsTerminal, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use pi_tui::component::{Component, UiEvent};
use pi_tui::components::{SettingItem, SettingsList, SettingsListOptions};
use pi_tui::terminal::{
    TerminalCapabilities, TerminalGuard, TerminalInput, Tui, Txn, install_panic_emergency_hook,
    write_emergency_restore_bytes,
};

use crate::core::config::{APP_NAME, CONFIG_DIR_NAME, get_agent_dir};
use crate::core::package_manager::{PackageManager, PackageManagerOptions};
use crate::core::resources::{
    PathMetadata, ResolvedPaths, ResolvedResource, SourceOrigin, SourceScope,
};
use crate::core::settings::{
    PackageSource, PackageSourceFilter, SettingsManager, SettingsManagerCreateOptions,
};
use crate::core::trust::{
    ProjectTrustStore, ResolveProjectTrustedOptions, resolve_project_trusted,
};
use crate::modes::interactive::theme::{self, settings_list_theme};

/// Options for the standalone config selector.
#[derive(Clone, Debug)]
pub struct ConfigSelectorOptions {
    /// Working directory.
    pub cwd: PathBuf,
    /// Agent config directory.
    pub agent_dir: PathBuf,
    /// Start writing to project settings (`-l`).
    pub write_project: bool,
    /// Invocation trust override (`-a` / `-na`).
    pub project_trust_override: Option<bool>,
    /// Offline flag (package resolve is local-only; kept for parity).
    pub offline: bool,
}

/// Parsed `pi config` flags.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct ConfigCommandFlags {
    /// `-l` / `--local`.
    pub local: bool,
    /// `-a` / `--approve` / `-na` / `--no-approve`.
    pub project_trust_override: Option<bool>,
    /// Unknown flag token.
    pub invalid_option: Option<String>,
    /// Unexpected positional.
    pub invalid_argument: Option<String>,
}

/// Parse `pi config` rest args (after the `config` token).
#[must_use]
pub fn parse_config_flags(rest: &[String]) -> ConfigCommandFlags {
    let mut flags = ConfigCommandFlags::default();
    for arg in rest {
        match arg.as_str() {
            "-l" | "--local" => flags.local = true,
            "-a" | "--approve" => flags.project_trust_override = Some(true),
            "-na" | "--no-approve" => flags.project_trust_override = Some(false),
            other if other.starts_with('-') => {
                flags.invalid_option.get_or_insert_with(|| other.to_owned());
            }
            other => {
                flags
                    .invalid_argument
                    .get_or_insert_with(|| other.to_owned());
            }
        }
    }
    flags
}

/// Run the resource-config TUI and return when the user exits.
///
/// # Errors
///
/// Returns a human-readable error when the terminal cannot be initialized or
/// settings/package resolution fails.
pub async fn select_config(options: ConfigSelectorOptions) -> Result<(), String> {
    if !io::stdout().is_terminal() || !io::stdin().is_terminal() {
        return Err(format!(
            "{APP_NAME} config requires an interactive terminal."
        ));
    }

    let project_trusted = resolve_project_trusted(ResolveProjectTrustedOptions {
        cwd: options.cwd.clone(),
        trust_store: &ProjectTrustStore::new(&options.agent_dir),
        trust_override: options.project_trust_override,
        default_project_trust: SettingsManager::create(
            &options.cwd,
            Some(&options.agent_dir),
            SettingsManagerCreateOptions::default().project_trusted(false),
        )
        .get_default_project_trust(),
        extension_hook: None,
        ui: None,
        on_extension_error: None,
    })
    .unwrap_or(false);

    if options.write_project && !project_trusted {
        return Err(
            "Project is not trusted. Use --approve to modify local resource config.".to_owned(),
        );
    }

    let settings = SettingsManager::create(
        &options.cwd,
        Some(&options.agent_dir),
        SettingsManagerCreateOptions::default().project_trusted(project_trusted),
    );

    let pm = PackageManager::with_offline(
        PackageManager::new(PackageManagerOptions::new(&options.cwd, &options.agent_dir)),
        options.offline,
    );

    let global_settings = SettingsManager::create(
        &options.cwd,
        Some(&options.agent_dir),
        SettingsManagerCreateOptions::default().project_trusted(false),
    );
    let global_paths = pm
        .resolve(&global_settings)
        .map_err(|error| error.to_string())?;
    let project_paths = if project_trusted {
        pm.resolve(&settings).map_err(|error| error.to_string())?
    } else {
        global_paths.clone()
    };

    let write_project = options.write_project && project_trusted;
    let active_paths = if write_project {
        &project_paths
    } else {
        &global_paths
    };
    let items = build_resource_items(active_paths, &options.agent_dir);

    theme::set_current(theme::dark());

    let closed = Arc::new(AtomicBool::new(false));
    let settings_slot = Arc::new(Mutex::new(settings));
    let resource_index = Arc::new(Mutex::new(build_resource_index(active_paths)));

    let closed_cancel = Arc::clone(&closed);
    let settings_for_change = Arc::clone(&settings_slot);
    let index_for_change = Arc::clone(&resource_index);
    let write_project_flag = write_project;

    let mut list = SettingsList::new(
        items,
        16,
        settings_list_theme(),
        move |id: &str, value: &str| {
            let enabled = value == "on";
            let Ok(mut settings) = settings_for_change.lock() else {
                return;
            };
            let Ok(index) = index_for_change.lock() else {
                return;
            };
            let Some(resource) = index.iter().find(|entry| entry.id == id) else {
                return;
            };
            if let Err(error) =
                apply_resource_toggle(&mut settings, resource, enabled, write_project_flag)
            {
                // Surface via stderr; the TUI keeps running so the user can exit.
                let mut stderr = io::stderr().lock();
                let _ = writeln!(stderr, "Error: {error}");
            }
        },
        move || {
            closed_cancel.store(true, Ordering::SeqCst);
        },
        &SettingsListOptions {
            enable_search: true,
        },
    );

    run_standalone_list(&mut list, &closed).await?;
    let _ = settings_slot;
    Ok(())
}

#[derive(Clone, Debug)]
struct ResourceEntry {
    id: String,
    path: String,
    resource_type: ResourceType,
    metadata: PathMetadata,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum ResourceType {
    Extensions,
    Skills,
    Prompts,
    Themes,
}

impl ResourceType {
    const fn as_str(self) -> &'static str {
        match self {
            Self::Extensions => "extensions",
            Self::Skills => "skills",
            Self::Prompts => "prompts",
            Self::Themes => "themes",
        }
    }

    const fn label(self) -> &'static str {
        match self {
            Self::Extensions => "Extensions",
            Self::Skills => "Skills",
            Self::Prompts => "Prompts",
            Self::Themes => "Themes",
        }
    }
}

fn build_resource_index(paths: &ResolvedPaths) -> Vec<ResourceEntry> {
    let mut out = Vec::new();
    for (kind, resources) in [
        (ResourceType::Extensions, &paths.extensions),
        (ResourceType::Skills, &paths.skills),
        (ResourceType::Prompts, &paths.prompts),
        (ResourceType::Themes, &paths.themes),
    ] {
        for resource in resources {
            out.push(ResourceEntry {
                id: resource_id(kind, resource),
                path: resource.path.clone(),
                resource_type: kind,
                metadata: resource.metadata.clone(),
            });
        }
    }
    out
}

fn build_resource_items(paths: &ResolvedPaths, agent_dir: &Path) -> Vec<SettingItem> {
    let mut items = Vec::new();
    for (kind, resources) in [
        (ResourceType::Extensions, &paths.extensions),
        (ResourceType::Skills, &paths.skills),
        (ResourceType::Prompts, &paths.prompts),
        (ResourceType::Themes, &paths.themes),
    ] {
        for resource in resources {
            let display = display_name(&resource.path);
            let group = group_label(&resource.metadata, agent_dir);
            items.push(SettingItem {
                id: resource_id(kind, resource),
                label: format!("[{}] {display}", kind.label()),
                description: Some(format!(
                    "{group} · {}",
                    if resource.enabled {
                        "enabled"
                    } else {
                        "disabled"
                    }
                )),
                current_value: if resource.enabled {
                    "on".to_owned()
                } else {
                    "off".to_owned()
                },
                values: Some(vec!["on".to_owned(), "off".to_owned()]),
                submenu: None,
            });
        }
    }
    if items.is_empty() {
        items.push(SettingItem {
            id: "__empty__".to_owned(),
            label: "No package resources found".to_owned(),
            description: Some(format!(
                "Install packages with `{APP_NAME} install <source>` or add paths under ~/{CONFIG_DIR_NAME}/agent/"
            )),
            current_value: String::new(),
            values: None,
            submenu: None,
        });
    }
    items
}

fn resource_id(kind: ResourceType, resource: &ResolvedResource) -> String {
    format!("{}:{}", kind.as_str(), resource.path)
}

fn display_name(path: &str) -> String {
    Path::new(path)
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .filter(|name| !name.is_empty())
        .unwrap_or_else(|| path.to_owned())
}

fn group_label(metadata: &PathMetadata, agent_dir: &Path) -> String {
    if metadata.origin == SourceOrigin::Package {
        return format!("{} ({:?})", metadata.source, metadata.scope);
    }
    if metadata.source == "auto" {
        return match metadata.scope {
            SourceScope::User => format!("User ({}/)", agent_dir.display()),
            SourceScope::Project => format!("Project ({CONFIG_DIR_NAME}/)"),
            SourceScope::Temporary => "Temporary".to_owned(),
        };
    }
    match metadata.scope {
        SourceScope::User => "User settings".to_owned(),
        SourceScope::Project => "Project settings".to_owned(),
        SourceScope::Temporary => "Temporary".to_owned(),
    }
}

fn apply_resource_toggle(
    settings: &mut SettingsManager,
    resource: &ResourceEntry,
    enabled: bool,
    write_project: bool,
) -> Result<(), String> {
    if resource.id == "__empty__" {
        return Ok(());
    }
    if resource.metadata.origin == SourceOrigin::TopLevel {
        toggle_top_level(settings, resource, enabled, write_project)
    } else {
        toggle_package(settings, resource, enabled, write_project)
    }
}

fn toggle_top_level(
    settings: &mut SettingsManager,
    resource: &ResourceEntry,
    enabled: bool,
    write_project: bool,
) -> Result<(), String> {
    let pattern = relative_pattern(resource);
    let disable = format!("-{pattern}");
    let enable = format!("+{pattern}");
    let mut current = current_top_level_paths(settings, resource.resource_type, write_project);
    current.retain(|entry| {
        let stripped = strip_pattern_prefix(entry);
        stripped != pattern
    });
    if enabled {
        current.push(enable);
    } else {
        current.push(disable);
    }
    set_top_level_paths(settings, resource.resource_type, current, write_project)
}

fn toggle_package(
    settings: &mut SettingsManager,
    resource: &ResourceEntry,
    enabled: bool,
    write_project: bool,
) -> Result<(), String> {
    let mut packages = if write_project {
        settings.get_project_settings().packages.unwrap_or_default()
    } else {
        settings.get_global_settings().packages.unwrap_or_default()
    };
    let Some(index) = packages
        .iter()
        .position(|pkg| package_source_string(pkg) == resource.metadata.source)
    else {
        return Ok(());
    };

    let pattern = package_relative_pattern(resource);
    let disable = format!("-{pattern}");
    let enable = format!("+{pattern}");

    let mut filter = match packages[index].clone() {
        PackageSource::Source(source) => PackageSourceFilter {
            source,
            ..PackageSourceFilter::default()
        },
        PackageSource::Filtered(filter) => filter,
    };

    {
        let patterns = package_filter_patterns_mut(&mut filter, resource.resource_type);
        patterns.retain(|entry| strip_pattern_prefix(entry) != pattern);
        if enabled {
            patterns.push(enable);
        } else {
            patterns.push(disable);
        }
        if patterns.is_empty() {
            *package_filter_patterns_opt_mut(&mut filter, resource.resource_type) = None;
        }
    }

    let has_filters = filter.extensions.as_ref().is_some_and(|v| !v.is_empty())
        || filter.skills.as_ref().is_some_and(|v| !v.is_empty())
        || filter.prompts.as_ref().is_some_and(|v| !v.is_empty())
        || filter.themes.as_ref().is_some_and(|v| !v.is_empty())
        || filter.autoload.is_some();
    packages[index] = if has_filters {
        PackageSource::Filtered(filter)
    } else {
        PackageSource::Source(filter.source)
    };

    if write_project {
        settings
            .set_project_packages(&packages)
            .map_err(|error| error.to_string())
    } else {
        settings.set_packages(&packages);
        Ok(())
    }
}

fn package_source_string(pkg: &PackageSource) -> String {
    match pkg {
        PackageSource::Source(source) => source.clone(),
        PackageSource::Filtered(filter) => filter.source.clone(),
    }
}

fn package_filter_patterns_mut(
    filter: &mut PackageSourceFilter,
    kind: ResourceType,
) -> &mut Vec<String> {
    let slot = package_filter_patterns_opt_mut(filter, kind);
    if slot.is_none() {
        *slot = Some(Vec::new());
    }
    match slot {
        Some(patterns) => patterns,
        None => unreachable!("package filter pattern vec inserted above"),
    }
}

fn package_filter_patterns_opt_mut(
    filter: &mut PackageSourceFilter,
    kind: ResourceType,
) -> &mut Option<Vec<String>> {
    match kind {
        ResourceType::Extensions => &mut filter.extensions,
        ResourceType::Skills => &mut filter.skills,
        ResourceType::Prompts => &mut filter.prompts,
        ResourceType::Themes => &mut filter.themes,
    }
}

fn current_top_level_paths(
    settings: &SettingsManager,
    kind: ResourceType,
    write_project: bool,
) -> Vec<String> {
    if write_project {
        let project = settings.get_project_settings();
        match kind {
            ResourceType::Extensions => project.extensions.unwrap_or_default(),
            ResourceType::Skills => project.skills.unwrap_or_default(),
            ResourceType::Prompts => project.prompts.unwrap_or_default(),
            ResourceType::Themes => project.themes.unwrap_or_default(),
        }
    } else {
        match kind {
            ResourceType::Extensions => settings.get_extension_paths(),
            ResourceType::Skills => settings.get_skill_paths(),
            ResourceType::Prompts => settings.get_prompt_template_paths(),
            ResourceType::Themes => settings.get_theme_paths(),
        }
    }
}

fn set_top_level_paths(
    settings: &mut SettingsManager,
    kind: ResourceType,
    paths: Vec<String>,
    write_project: bool,
) -> Result<(), String> {
    if write_project {
        match kind {
            ResourceType::Extensions => settings
                .set_project_extension_paths(paths)
                .map_err(|error| error.to_string()),
            ResourceType::Skills => settings
                .set_project_skill_paths(paths)
                .map_err(|error| error.to_string()),
            ResourceType::Prompts => settings
                .set_project_prompt_template_paths(paths)
                .map_err(|error| error.to_string()),
            ResourceType::Themes => settings
                .set_project_theme_paths(paths)
                .map_err(|error| error.to_string()),
        }
    } else {
        match kind {
            ResourceType::Extensions => settings.set_extension_paths(paths),
            ResourceType::Skills => settings.set_skill_paths(paths),
            ResourceType::Prompts => settings.set_prompt_template_paths(paths),
            ResourceType::Themes => settings.set_theme_paths(paths),
        }
        Ok(())
    }
}

fn relative_pattern(resource: &ResourceEntry) -> String {
    if let Some(base) = resource.metadata.base_dir.as_deref() {
        let base = Path::new(base);
        let path = Path::new(&resource.path);
        if let Ok(rel) = path.strip_prefix(base) {
            return rel.to_string_lossy().replace('\\', "/");
        }
    }
    Path::new(&resource.path).file_name().map_or_else(
        || resource.path.clone(),
        |name| name.to_string_lossy().into_owned(),
    )
}

fn package_relative_pattern(resource: &ResourceEntry) -> String {
    relative_pattern(resource)
}

fn strip_pattern_prefix(entry: &str) -> &str {
    entry
        .strip_prefix('!')
        .or_else(|| entry.strip_prefix('+'))
        .or_else(|| entry.strip_prefix('-'))
        .unwrap_or(entry)
}

async fn run_standalone_list(list: &mut SettingsList, closed: &AtomicBool) -> Result<(), String> {
    let size = crossterm::terminal::size().unwrap_or((80, 24));
    let mut guard = TerminalGuard::new(io::stdout());
    guard.set_viewport_bottom_row(size.1.saturating_sub(1));
    let emergency = guard.emergency_flag();
    {
        let restore_writer = Arc::new(Mutex::new(io::stdout()));
        install_panic_emergency_hook(
            Arc::clone(&emergency),
            Arc::new(move || {
                if let Ok(mut writer) = restore_writer.lock() {
                    let _ = write_emergency_restore_bytes(&mut *writer);
                }
            }),
        );
    }
    guard
        .activate(!cfg!(windows))
        .map_err(|error| format!("terminal activation failed: {error}"))?;

    let mut tui = Tui::new(
        io::stdout(),
        ratatui::layout::Size::new(size.0, size.1),
        ratatui::layout::Position::ORIGIN,
        size.1.max(1),
        TerminalCapabilities::detect(),
    )
    .map_err(|error| format!("tui initialization failed: {error}"))?;

    let mut input = TerminalInput::spawn();
    tui.commit(Txn::Frame, list)
        .map_err(|error| format!("tui paint failed: {error}"))?;

    while !closed.load(Ordering::SeqCst) {
        let Some(event) = input.recv().await else {
            break;
        };
        if let UiEvent::Key(key) = &event
            && key
                .modifiers
                .contains(crossterm::event::KeyModifiers::CONTROL)
            && matches!(key.code, crossterm::event::KeyCode::Char('c'))
        {
            break;
        }
        if let UiEvent::Resize { width, height } = event {
            tui.note_resize(width, height);
            guard.set_viewport_bottom_row(height.saturating_sub(1));
            list.invalidate();
            tui.commit(Txn::Frame, list)
                .map_err(|error| format!("tui paint failed: {error}"))?;
            continue;
        }
        let result = list.handle_event(&event);
        if closed.load(Ordering::SeqCst) {
            break;
        }
        if result.needs_render() || result.is_handled() {
            tui.commit(Txn::Frame, list)
                .map_err(|error| format!("tui paint failed: {error}"))?;
        }
    }

    drop(input);
    drop(tui);
    Ok(())
}

/// Convenience constructor using process cwd / agent dir.
#[must_use]
pub fn options_from_process(
    local: bool,
    project_trust_override: Option<bool>,
    offline: bool,
) -> ConfigSelectorOptions {
    ConfigSelectorOptions {
        cwd: std::env::current_dir().unwrap_or_else(|_| PathBuf::from(".")),
        agent_dir: get_agent_dir(),
        write_project: local,
        project_trust_override,
        offline,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_config_flags_local_and_trust() {
        let flags = parse_config_flags(&["-l".to_owned(), "--approve".to_owned()]);
        assert!(flags.local);
        assert_eq!(flags.project_trust_override, Some(true));
        assert!(flags.invalid_option.is_none());
        assert!(flags.invalid_argument.is_none());
    }

    #[test]
    fn parse_config_flags_rejects_unknown() {
        let flags = parse_config_flags(&["--bogus".to_owned(), "extra".to_owned()]);
        assert_eq!(flags.invalid_option.as_deref(), Some("--bogus"));
        assert_eq!(flags.invalid_argument.as_deref(), Some("extra"));
    }

    #[test]
    fn strip_pattern_prefix_handles_markers() {
        assert_eq!(strip_pattern_prefix("+foo"), "foo");
        assert_eq!(strip_pattern_prefix("-foo"), "foo");
        assert_eq!(strip_pattern_prefix("!foo"), "foo");
        assert_eq!(strip_pattern_prefix("foo"), "foo");
    }
}
