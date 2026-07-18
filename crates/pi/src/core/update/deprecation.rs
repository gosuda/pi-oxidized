//! Pure deprecation warning data consumed by startup UI.
//!
//! This intentionally does not port the unused process-global `warnDeprecation`
//! utility. Detection is idempotent and rendering belongs to interactive mode.

/// Extension migration guide shown after deprecated layouts are detected.
pub const MIGRATION_GUIDE_URL: &str = "https://github.com/earendil-works/pi-mono/blob/main/packages/coding-agent/CHANGELOG.md#extensions-migration";
/// Extension documentation shown with migration warnings.
pub const EXTENSIONS_DOC_URL: &str =
    "https://github.com/earendil-works/pi-mono/blob/main/packages/coding-agent/docs/extensions.md";

/// Stable warning identifiers for callers and serialized fixtures.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum DeprecationCode {
    /// Legacy `hooks/` directory.
    HooksDirectory,
    /// Legacy custom files in `tools/`.
    CustomToolsDirectory,
}

/// Data-only deprecation warning.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct DeprecationWarning {
    /// Stable warning kind.
    pub code: DeprecationCode,
    /// Human-readable compatibility text.
    pub message: String,
    /// Migration guide URL.
    pub migration_guide_url: &'static str,
    /// Current extension documentation URL.
    pub documentation_url: &'static str,
}

/// Build deterministic warnings for a global or project scope.
#[must_use]
pub fn extension_layout_warnings(
    label: &str,
    hooks_directory_exists: bool,
    custom_tools_exist: bool,
) -> Vec<DeprecationWarning> {
    let mut warnings = Vec::with_capacity(2);
    if hooks_directory_exists {
        warnings.push(DeprecationWarning {
            code: DeprecationCode::HooksDirectory,
            message: format!(
                "{label} hooks/ directory found. Hooks have been renamed to extensions."
            ),
            migration_guide_url: MIGRATION_GUIDE_URL,
            documentation_url: EXTENSIONS_DOC_URL,
        });
    }
    if custom_tools_exist {
        warnings.push(DeprecationWarning {
            code: DeprecationCode::CustomToolsDirectory,
            message: format!(
                "{label} tools/ directory contains custom tools. Custom tools have been merged into extensions."
            ),
            migration_guide_url: MIGRATION_GUIDE_URL,
            documentation_url: EXTENSIONS_DOC_URL,
        });
    }
    warnings
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn warning_data_is_deterministic_and_has_no_global_state() {
        let first = extension_layout_warnings("Global", true, true);
        let second = extension_layout_warnings("Global", true, true);
        assert_eq!(first, second);
        assert_eq!(first.len(), 2);
        assert_eq!(first[0].code, DeprecationCode::HooksDirectory);
        assert!(
            first[1]
                .message
                .contains("Custom tools have been merged into extensions.")
        );
    }

    #[test]
    fn no_warnings_when_both_directories_absent() {
        let warnings = extension_layout_warnings("Global", false, false);
        assert!(warnings.is_empty());
    }

    #[test]
    fn only_hooks_warning_when_hooks_present() {
        let warnings = extension_layout_warnings("Project", true, false);
        assert_eq!(warnings.len(), 1);
        assert_eq!(warnings[0].code, DeprecationCode::HooksDirectory);
        assert!(
            warnings[0]
                .message
                .contains("Project hooks/ directory found.")
        );
        assert!(
            warnings[0]
                .message
                .contains("Hooks have been renamed to extensions.")
        );
    }

    #[test]
    fn only_custom_tools_warning_when_tools_present() {
        let warnings = extension_layout_warnings("Global", false, true);
        assert_eq!(warnings.len(), 1);
        assert_eq!(warnings[0].code, DeprecationCode::CustomToolsDirectory);
        assert!(
            warnings[0]
                .message
                .contains("Global tools/ directory contains custom tools.")
        );
        assert!(
            warnings[0]
                .message
                .contains("Custom tools have been merged into extensions.")
        );
    }

    #[test]
    fn warnings_are_ordered_hooks_before_custom_tools() {
        let warnings = extension_layout_warnings("Global", true, true);
        assert_eq!(warnings.len(), 2);
        assert_eq!(warnings[0].code, DeprecationCode::HooksDirectory);
        assert_eq!(warnings[1].code, DeprecationCode::CustomToolsDirectory);
    }

    #[test]
    fn warning_urls_point_to_canonical_docs() {
        let warnings = extension_layout_warnings("Global", true, true);
        for warning in &warnings {
            assert_eq!(warning.migration_guide_url, MIGRATION_GUIDE_URL);
            assert_eq!(warning.documentation_url, EXTENSIONS_DOC_URL);
            assert!(warning.migration_guide_url.starts_with("https://"));
            assert!(warning.documentation_url.starts_with("https://"));
        }
    }

    #[test]
    fn warning_label_appears_in_message() {
        let warnings = extension_layout_warnings("MyProject", true, false);
        assert!(!warnings.is_empty());
        assert!(warnings[0].message.starts_with("MyProject "));
    }
}
