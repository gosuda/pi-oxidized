//! Theme path collection (raw JSON load only; Theme type stays Phase 4).
//!
//! Port of theme path loading from
//! `.references/pi/packages/coding-agent/src/core/resource-loader.ts`.

use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};

use serde_json::Value;

use crate::core::config::{PathInputOptions, resolve_path_with};
use crate::core::resources::diagnostics::{ResourceCollision, ResourceDiagnostic, ResourceType};
use crate::core::resources::source_info::{
    SourceInfo, SyntheticSourceInfoOptions, create_synthetic_source_info,
};

/// Loaded theme JSON with provenance (Phase 3 raw form).
#[derive(Clone, Debug, PartialEq)]
pub struct LoadedTheme {
    /// Theme name from JSON `name`, or `"unnamed"`.
    pub name: String,
    /// Absolute source path.
    pub source_path: String,
    /// Provenance.
    pub source_info: SourceInfo,
    /// Raw JSON document.
    pub raw: Value,
}

/// Result of loading themes from paths.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct LoadThemesResult {
    /// Deduped themes (first name wins).
    pub themes: Vec<LoadedTheme>,
    /// Load / collision diagnostics.
    pub diagnostics: Vec<ResourceDiagnostic>,
}

/// Options for [`load_themes`].
#[derive(Clone, Debug)]
pub struct LoadThemesOptions {
    /// Working directory for relative path resolution.
    pub cwd: PathBuf,
    /// Explicit theme paths (files or directories).
    pub theme_paths: Vec<String>,
}

/// Load themes from paths/dirs (`.json`, non-recursive).
///
/// Collision key is `name ?? "unnamed"`. Exact diagnostic messages match TS.
#[must_use]
pub fn load_themes(options: &LoadThemesOptions) -> LoadThemesResult {
    let mut themes = Vec::new();
    let mut diagnostics = Vec::new();
    let resolved_cwd = resolve_path_with(
        &path_to_string(&options.cwd),
        Path::new("."),
        PathInputOptions::new(),
    );

    for raw in &options.theme_paths {
        let resolved = resolve_path_with(raw, &resolved_cwd, PathInputOptions::new().trim(true));
        if !resolved.exists() {
            diagnostics.push(ResourceDiagnostic::warning(
                "theme path does not exist",
                Some(path_to_string(&resolved)),
            ));
            continue;
        }
        match fs::metadata(&resolved) {
            Ok(meta) if meta.is_dir() => {
                load_themes_from_dir(&resolved, &mut themes, &mut diagnostics);
            }
            Ok(meta)
                if meta.is_file()
                    && resolved
                        .extension()
                        .is_some_and(|ext| ext.eq_ignore_ascii_case("json")) =>
            {
                load_theme_from_file(&resolved, &mut themes, &mut diagnostics);
            }
            Ok(_) => {
                diagnostics.push(ResourceDiagnostic::warning(
                    "theme path is not a json file",
                    Some(path_to_string(&resolved)),
                ));
            }
            Err(error) => {
                diagnostics.push(ResourceDiagnostic::warning(
                    error.to_string(),
                    Some(path_to_string(&resolved)),
                ));
            }
        }
    }

    let deduped = dedupe_themes(themes);
    diagnostics.extend(deduped.diagnostics);
    LoadThemesResult {
        themes: deduped.themes,
        diagnostics,
    }
}

/// Load themes from a directory (non-recursive `.json` files).
pub fn load_themes_from_dir(
    dir: &Path,
    themes: &mut Vec<LoadedTheme>,
    diagnostics: &mut Vec<ResourceDiagnostic>,
) {
    if !dir.exists() {
        return;
    }
    let Ok(read_dir) = fs::read_dir(dir) else {
        diagnostics.push(ResourceDiagnostic::warning(
            "failed to read theme directory",
            Some(path_to_string(dir)),
        ));
        return;
    };
    let mut entries: Vec<_> = read_dir.filter_map(Result::ok).collect();
    entries.sort_by_key(std::fs::DirEntry::file_name);
    for entry in entries {
        let full_path = entry.path();
        let Ok(meta) = fs::metadata(&full_path) else {
            continue;
        };
        if meta.is_file()
            && full_path
                .extension()
                .is_some_and(|ext| ext.eq_ignore_ascii_case("json"))
        {
            load_theme_from_file(&full_path, themes, diagnostics);
        }
    }
}

/// Load a single theme JSON file.
pub fn load_theme_from_file(
    file_path: &Path,
    themes: &mut Vec<LoadedTheme>,
    diagnostics: &mut Vec<ResourceDiagnostic>,
) {
    match load_theme_value(file_path) {
        Ok(theme) => themes.push(theme),
        Err(message) => {
            diagnostics.push(ResourceDiagnostic::warning(
                message,
                Some(path_to_string(file_path)),
            ));
        }
    }
}

fn load_theme_value(file_path: &Path) -> Result<LoadedTheme, String> {
    let content = fs::read_to_string(file_path).map_err(|error| error.to_string())?;
    let raw: Value = serde_json::from_str(&content).map_err(|error| error.to_string())?;
    let name = raw
        .get("name")
        .and_then(Value::as_str)
        .unwrap_or("unnamed")
        .to_owned();
    let source_path = path_to_string(file_path);
    let source_info = create_synthetic_source_info(
        source_path.clone(),
        SyntheticSourceInfoOptions {
            source: "local".into(),
            scope: None,
            origin: None,
            base_dir: file_path.parent().map(path_to_string),
        },
    );
    Ok(LoadedTheme {
        name,
        source_path,
        source_info,
        raw,
    })
}

fn dedupe_themes(themes: Vec<LoadedTheme>) -> LoadThemesResult {
    let mut winners: Vec<LoadedTheme> = Vec::new();
    let mut index_by_name: HashMap<String, usize> = HashMap::new();
    let mut diagnostics = Vec::new();
    for theme in themes {
        // TS: `const name = t.name ?? "unnamed"` — empty string is kept.
        let name = theme.name.clone();
        if let Some(&winner_idx) = index_by_name.get(&name) {
            let winner = &winners[winner_idx];
            diagnostics.push(ResourceDiagnostic::collision(
                format!("name \"{name}\" collision"),
                Some(theme.source_path.clone()),
                ResourceCollision {
                    resource_type: ResourceType::Theme,
                    name: name.clone(),
                    winner_path: winner.source_path.clone(),
                    loser_path: theme.source_path.clone(),
                    winner_source: None,
                    loser_source: None,
                },
            ));
        } else {
            index_by_name.insert(name, winners.len());
            winners.push(theme);
        }
    }
    LoadThemesResult {
        themes: winners,
        diagnostics,
    }
}

fn path_to_string(path: &Path) -> String {
    path.to_string_lossy().into_owned()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{SystemTime, UNIX_EPOCH};

    fn temp_root(label: &str) -> std::io::Result<PathBuf> {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_or(0, |duration| duration.as_nanos());
        let root = std::env::temp_dir().join(format!("pi-themes-{label}-{nanos}"));
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(&root)?;
        Ok(root)
    }

    #[test]
    fn load_themes_nonrecursive_and_collision() -> std::io::Result<()> {
        let root = temp_root("themes")?;
        let dir = root.join("themes");
        fs::create_dir_all(dir.join("nested"))?;
        fs::write(dir.join("a.json"), r#"{"name":"alpha"}"#)?;
        fs::write(dir.join("b.json"), r#"{"name":"alpha"}"#)?;
        fs::write(dir.join("nested").join("c.json"), r#"{"name":"nested"}"#)?;
        let result = load_themes(&LoadThemesOptions {
            cwd: root.clone(),
            theme_paths: vec![path_to_string(&dir)],
        });
        assert_eq!(result.themes.len(), 1);
        assert!(
            result
                .diagnostics
                .iter()
                .any(|d| d.message == "name \"alpha\" collision")
        );
        assert!(!result.themes.iter().any(|t| t.name == "nested"));
        let _ = fs::remove_dir_all(root);
        Ok(())
    }

    #[test]
    fn missing_theme_path_diagnostic() -> std::io::Result<()> {
        let root = temp_root("missing-theme")?;
        let result = load_themes(&LoadThemesOptions {
            cwd: root.clone(),
            theme_paths: vec![path_to_string(&root.join("nope.json"))],
        });
        assert!(result.themes.is_empty());
        assert!(
            result
                .diagnostics
                .iter()
                .any(|d| d.message == "theme path does not exist")
        );
        let _ = fs::remove_dir_all(root);
        Ok(())
    }

    #[test]
    fn unnamed_collision_key() -> std::io::Result<()> {
        let root = temp_root("unnamed")?;
        let a = root.join("a.json");
        let b = root.join("b.json");
        fs::write(&a, r"{}")?;
        fs::write(&b, r"{}")?;
        let result = load_themes(&LoadThemesOptions {
            cwd: root.clone(),
            theme_paths: vec![path_to_string(&a), path_to_string(&b)],
        });
        assert_eq!(result.themes.len(), 1);
        assert_eq!(result.themes[0].name, "unnamed");
        assert!(
            result
                .diagnostics
                .iter()
                .any(|d| d.message == "name \"unnamed\" collision")
        );
        let _ = fs::remove_dir_all(root);
        Ok(())
    }
}
