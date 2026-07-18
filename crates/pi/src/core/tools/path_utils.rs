//! Path expansion, cwd resolution, and macOS path-variant fallback for tool
//! file arguments.
//!
//! Ports `.references/pi/packages/coding-agent/src/core/tools/path-utils.ts`
//! together with the `normalizePath` / `resolvePath` helpers from
//! `.references/pi/packages/coding-agent/src/utils/paths.ts`. Inputs are
//! normalized with Unicode-space folding and an optional `@` strip, `~` and
//! `file://` expansion, then resolved against a cwd. When the resolved path
//! does not exist, the read resolvers try the macOS variants in order:
//! narrow no-break space before AM/PM, NFD normalization, curly apostrophe,
//! and NFD + curly apostrophe combined.

use std::path::{Component, Path, PathBuf};

use thiserror::Error;
use unicode_normalization::UnicodeNormalization;

/// Narrow no-break space (TypeScript `NARROW_NO_BREAK_SPACE`); macOS uses it
/// before the AM/PM marker in localized screenshot names.
const NARROW_NO_BREAK_SPACE: char = '\u{202F}';

/// Errors produced while expanding or resolving tool paths.
#[derive(Debug, Error)]
pub enum PathResolveError {
    /// No home directory could be determined for `~` expansion.
    #[error("failed to determine home directory for `~` expansion")]
    HomeDirectoryUnavailable,
    /// A `file://` URL could not be converted to a filesystem path (Node
    /// `fileURLToPath` throws for the same inputs).
    #[error("invalid file:// path `{input}`: {reason}")]
    InvalidFileUrl {
        /// The original `file://` input.
        input: String,
        /// Why the URL could not be converted.
        reason: String,
    },
    /// The process working directory was required but unavailable.
    #[error("failed to determine current directory: {source}")]
    CurrentDirectory {
        /// Underlying I/O failure.
        source: std::io::Error,
    },
}

/// Unicode space variants folded to a plain space
/// (TypeScript `UNICODE_SPACES`): U+00A0, U+2000..=U+200A, U+202F, U+205F,
/// U+3000.
fn is_unicode_space(ch: char) -> bool {
    matches!(
        ch,
        '\u{00A0}' | '\u{2000}'..='\u{200A}' | '\u{202F}' | '\u{205F}' | '\u{3000}'
    )
}

/// Node `path.isAbsolute`. On Windows Node also treats rooted paths without a
/// drive prefix (`\foo`) as absolute; Rust's [`Path::is_absolute`] does not.
fn is_absolute_node(path: &Path) -> bool {
    #[cfg(windows)]
    {
        path.is_absolute() || path.has_root()
    }
    #[cfg(not(windows))]
    {
        path.is_absolute()
    }
}

/// Lexically normalize a path the way Node `path.resolve` does: `.` segments
/// dropped, `..` popping the previous segment but never escaping the root.
/// No filesystem access.
pub(crate) fn lexical_normalize(path: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for component in path.components() {
        match component {
            Component::Prefix(prefix) => out.push(prefix.as_os_str()),
            Component::RootDir => out.push(component.as_os_str()),
            Component::CurDir => {}
            Component::ParentDir => {
                // `pop` returns false at the root, which matches Node keeping
                // the resolution anchored there.
                let _ = out.pop();
            }
            Component::Normal(part) => out.push(part),
        }
    }
    out
}

/// Node `path.resolve(path)` against the process working directory:
/// absolute-ize, then lexically normalize. No filesystem access.
pub(crate) fn resolve_lexically_absolute(path: &Path) -> std::io::Result<PathBuf> {
    if is_absolute_node(path) {
        Ok(lexical_normalize(path))
    } else {
        Ok(lexical_normalize(&std::env::current_dir()?.join(path)))
    }
}

fn home_dir_string() -> Result<String, PathResolveError> {
    let home = dirs::home_dir().ok_or(PathResolveError::HomeDirectoryUnavailable)?;
    Ok(home.to_string_lossy().into_owned())
}

/// Convert a `file://` URL to a path (Node `fileURLToPath`). Non-local hosts
/// are rejected, matching Node's `ERR_INVALID_FILE_URL_HOST`.
fn file_url_to_path(input: &str) -> Result<String, PathResolveError> {
    let url = url::Url::parse(input).map_err(|error| PathResolveError::InvalidFileUrl {
        input: input.to_owned(),
        reason: error.to_string(),
    })?;
    let path = url
        .to_file_path()
        .map_err(|()| PathResolveError::InvalidFileUrl {
            input: input.to_owned(),
            reason: "file URL host must be empty or `localhost`".to_owned(),
        })?;
    Ok(path.to_string_lossy().into_owned())
}

/// `normalizePath` from `utils/paths.ts` with the option set the tools use:
/// Unicode-space folding and `@` stripping are option-dependent; `~`
/// expansion and `file://` conversion always apply.
fn normalize_path_with(
    input: &str,
    normalize_unicode_spaces: bool,
    strip_at_prefix: bool,
) -> Result<String, PathResolveError> {
    let mut normalized = if normalize_unicode_spaces {
        input
            .chars()
            .map(|ch| if is_unicode_space(ch) { ' ' } else { ch })
            .collect::<String>()
    } else {
        input.to_owned()
    };
    if strip_at_prefix && normalized.starts_with('@') {
        normalized = normalized[1..].to_owned();
    }

    if normalized == "~" {
        return home_dir_string();
    }
    if normalized.starts_with("~/") || (cfg!(windows) && normalized.starts_with("~\\")) {
        let home = home_dir_string()?;
        let joined = Path::new(&home).join(&normalized[2..]);
        return Ok(lexical_normalize(&joined).to_string_lossy().into_owned());
    }

    if normalized.starts_with("file://") {
        return file_url_to_path(&normalized);
    }

    Ok(normalized)
}

/// Expand a user-supplied path without resolving it against a cwd
/// (TypeScript `expandPath`): Unicode-space folding, `@` strip, `~`
/// expansion, and `file://` conversion.
///
/// # Errors
///
/// Returns [`PathResolveError::HomeDirectoryUnavailable`] for `~` without a
/// home directory and [`PathResolveError::InvalidFileUrl`] for an unusable
/// `file://` URL.
pub fn expand_path(file_path: &str) -> Result<String, PathResolveError> {
    normalize_path_with(file_path, true, true)
}

/// Resolve a path relative to `cwd` (TypeScript `resolveToCwd`). The input
/// gets Unicode-space folding, `@` strip, `~` expansion, and `file://`
/// conversion; the cwd is itself `~`-expanded but not space-folded, matching
/// `resolvePath(filePath, cwd, { normalizeUnicodeSpaces: true, stripAtPrefix: true })`.
///
/// # Errors
///
/// Returns the expansion errors of [`expand_path`], or
/// [`PathResolveError::CurrentDirectory`] when a relative base forces a
/// working-directory lookup.
pub fn resolve_to_cwd(file_path: &str, cwd: &str) -> Result<String, PathResolveError> {
    let normalized = normalize_path_with(file_path, true, true)?;
    let normalized_base = normalize_path_with(cwd, false, false)?;

    let normalized_path = Path::new(&normalized);
    let joined = if is_absolute_node(normalized_path) {
        PathBuf::from(normalized_path)
    } else {
        Path::new(&normalized_base).join(normalized_path)
    };
    let absolute = if is_absolute_node(&joined) {
        joined
    } else {
        std::env::current_dir()
            .map_err(|source| PathResolveError::CurrentDirectory { source })?
            .join(joined)
    };
    Ok(lexical_normalize(&absolute).to_string_lossy().into_owned())
}

/// Synchronous existence check (TypeScript `accessSync(F_OK)`; any error
/// means "does not exist").
fn file_exists(file_path: &str) -> bool {
    Path::new(file_path).try_exists().unwrap_or(false)
}

/// Async existence check (TypeScript `pathExists`).
pub async fn path_exists(file_path: &str) -> bool {
    tokio::fs::try_exists(file_path).await.unwrap_or(false)
}

/// Replace every ` AM.` / ` PM.` (any letter case) with the narrow no-break
/// space variant (TypeScript `tryMacOSScreenshotPath`, regex `/ (AM|PM)\./gi`).
fn try_macos_screenshot_path(file_path: &str) -> String {
    let bytes = file_path.as_bytes();
    let mut out = String::with_capacity(file_path.len() + 8);
    let mut index = 0;
    let mut copied = 0;
    while index + 3 < bytes.len() {
        // The pattern is pure ASCII, so byte-level matching cannot split a
        // multi-byte character.
        if bytes[index] == b' '
            && matches!(bytes[index + 1] | 0x20, b'a' | b'p')
            && (bytes[index + 2] | 0x20) == b'm'
            && bytes[index + 3] == b'.'
        {
            out.push_str(&file_path[copied..index]);
            out.push(NARROW_NO_BREAK_SPACE);
            out.push_str(&file_path[index + 1..index + 4]);
            index += 4;
            copied = index;
        } else {
            index += 1;
        }
    }
    out.push_str(&file_path[copied..]);
    out
}

/// NFD-decomposed variant; macOS stores filenames in NFD form while users
/// usually type NFC (TypeScript `tryNFDVariant`).
fn try_nfd_variant(file_path: &str) -> String {
    file_path.nfd().collect()
}

/// Curly-apostrophe variant of straight quotes
/// (TypeScript `tryCurlyQuoteVariant`).
fn try_curly_quote_variant(file_path: &str) -> String {
    file_path.replace('\'', "\u{2019}")
}

/// Resolve a read path, falling back through the macOS variants when the
/// plainly resolved path does not exist (TypeScript `resolveReadPath`).
/// Returns the plainly resolved path when no variant exists either.
///
/// # Errors
///
/// Returns the expansion errors of [`resolve_to_cwd`].
pub fn resolve_read_path(file_path: &str, cwd: &str) -> Result<String, PathResolveError> {
    let resolved = resolve_to_cwd(file_path, cwd)?;

    if file_exists(&resolved) {
        return Ok(resolved);
    }

    // Narrow no-break space before AM/PM (macOS screenshot names).
    let am_pm_variant = try_macos_screenshot_path(&resolved);
    if am_pm_variant != resolved && file_exists(&am_pm_variant) {
        return Ok(am_pm_variant);
    }

    // NFD variant (macOS stores filenames decomposed).
    let nfd_variant = try_nfd_variant(&resolved);
    if nfd_variant != resolved && file_exists(&nfd_variant) {
        return Ok(nfd_variant);
    }

    // Curly apostrophe (macOS localized screenshot names).
    let curly_variant = try_curly_quote_variant(&resolved);
    if curly_variant != resolved && file_exists(&curly_variant) {
        return Ok(curly_variant);
    }

    // Combined NFD + curly apostrophe (e.g. French "Capture d’écran").
    let nfd_curly_variant = try_curly_quote_variant(&nfd_variant);
    if nfd_curly_variant != resolved && file_exists(&nfd_curly_variant) {
        return Ok(nfd_curly_variant);
    }

    Ok(resolved)
}

/// Async form of [`resolve_read_path`] (TypeScript `resolveReadPathAsync`).
///
/// # Errors
///
/// Returns the expansion errors of [`resolve_to_cwd`].
pub async fn resolve_read_path_async(
    file_path: &str,
    cwd: &str,
) -> Result<String, PathResolveError> {
    let resolved = resolve_to_cwd(file_path, cwd)?;

    if path_exists(&resolved).await {
        return Ok(resolved);
    }

    // Narrow no-break space before AM/PM (macOS screenshot names).
    let am_pm_variant = try_macos_screenshot_path(&resolved);
    if am_pm_variant != resolved && path_exists(&am_pm_variant).await {
        return Ok(am_pm_variant);
    }

    // NFD variant (macOS stores filenames decomposed).
    let nfd_variant = try_nfd_variant(&resolved);
    if nfd_variant != resolved && path_exists(&nfd_variant).await {
        return Ok(nfd_variant);
    }

    // Curly apostrophe (macOS localized screenshot names).
    let curly_variant = try_curly_quote_variant(&resolved);
    if curly_variant != resolved && path_exists(&curly_variant).await {
        return Ok(curly_variant);
    }

    // Combined NFD + curly apostrophe (e.g. French "Capture d’écran").
    let nfd_curly_variant = try_curly_quote_variant(&nfd_variant);
    if nfd_curly_variant != resolved && path_exists(&nfd_curly_variant).await {
        return Ok(nfd_curly_variant);
    }

    Ok(resolved)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::error::Error;

    type TestResult = Result<(), Box<dyn Error + Send + Sync>>;

    fn cwd_str(dir: &Path) -> String {
        dir.to_string_lossy().into_owned()
    }

    #[test]
    fn expand_path_folds_unicode_spaces_and_strips_at() -> TestResult {
        assert_eq!(expand_path("my\u{00A0}file.txt")?, "my file.txt");
        assert_eq!(expand_path("a\u{2009}b\u{3000}c")?, "a b c");
        assert_eq!(expand_path("@src/main.ts")?, "src/main.ts");
        // The narrow no-break space folds like any other Unicode space here;
        // it is only special inside macOS screenshot *variants*.
        assert_eq!(expand_path("1.23.45\u{202F}PM.png")?, "1.23.45 PM.png");
        Ok(())
    }

    #[test]
    fn expand_path_expands_tilde_against_home() -> TestResult {
        let Some(home) = dirs::home_dir() else {
            // No home directory in this environment: nothing to compare.
            return Ok(());
        };
        let home = home.to_string_lossy().into_owned();
        assert_eq!(expand_path("~")?, home);
        assert_eq!(
            expand_path("~/Documents/file.txt")?,
            lexical_normalize(&Path::new(&home).join("Documents/file.txt"))
                .to_string_lossy()
                .into_owned()
        );
        // Tilde that does not lead the path is untouched.
        assert_eq!(expand_path("a/~/b")?, "a/~/b");
        Ok(())
    }

    #[test]
    fn expand_path_converts_file_urls() -> TestResult {
        assert_eq!(expand_path("file:///etc/hostname")?, "/etc/hostname");
        assert_eq!(expand_path("file:///tmp/my%20file")?, "/tmp/my file");
        Ok(())
    }

    #[test]
    fn resolve_to_cwd_joins_relative_and_normalizes() -> TestResult {
        assert_eq!(resolve_to_cwd("src/./a.rs", "/repo")?, "/repo/src/a.rs");
        assert_eq!(resolve_to_cwd("../lib", "/repo/pkg")?, "/repo/lib");
        assert_eq!(resolve_to_cwd("/abs/x", "/repo")?, "/abs/x");
        // `..` cannot escape the root.
        assert_eq!(resolve_to_cwd("../../x", "/")?, "/x");
        // Unicode spaces and @ are folded in the input only.
        assert_eq!(resolve_to_cwd("@my\u{00A0}f", "/repo")?, "/repo/my f");
        Ok(())
    }

    #[test]
    fn resolve_read_path_returns_plain_resolution_when_present() -> TestResult {
        let dir = tempfile::tempdir()?;
        let file = dir.path().join("plain.txt");
        std::fs::write(&file, b"data")?;
        let resolved = resolve_read_path("plain.txt", &cwd_str(dir.path()))?;
        assert_eq!(Path::new(&resolved), file.as_path());
        Ok(())
    }

    #[test]
    fn resolve_read_path_finds_narrow_no_break_space_variant() -> TestResult {
        let dir = tempfile::tempdir()?;
        let macos_name = "Screen Shot 2026-01-01 at 1.23.45\u{202F}PM.png";
        std::fs::write(dir.path().join(macos_name), b"img")?;
        let resolved = resolve_read_path(
            "Screen Shot 2026-01-01 at 1.23.45 PM.png",
            &cwd_str(dir.path()),
        )?;
        assert!(resolved.ends_with("1.23.45\u{202F}PM.png"));
        assert_eq!(Path::new(&resolved), dir.path().join(macos_name).as_path());
        Ok(())
    }

    #[test]
    fn resolve_read_path_finds_nfd_variant() -> TestResult {
        let dir = tempfile::tempdir()?;
        // Create the file under its NFD name, query with NFC.
        let nfd_name: String = "café.txt".nfd().collect();
        std::fs::write(dir.path().join(&nfd_name), b"data")?;
        let resolved = resolve_read_path("café.txt", &cwd_str(dir.path()))?;
        assert_eq!(Path::new(&resolved), dir.path().join(&nfd_name).as_path());
        Ok(())
    }

    #[test]
    fn resolve_read_path_finds_curly_quote_variant() -> TestResult {
        let dir = tempfile::tempdir()?;
        let curly_name = "Capture d\u{2019}écran.png";
        std::fs::write(dir.path().join(curly_name), b"img")?;
        let resolved = resolve_read_path("Capture d'écran.png", &cwd_str(dir.path()))?;
        assert_eq!(Path::new(&resolved), dir.path().join(curly_name).as_path());
        Ok(())
    }

    #[test]
    fn resolve_read_path_finds_combined_nfd_curly_variant() -> TestResult {
        let dir = tempfile::tempdir()?;
        // File on disk: NFD + curly apostrophe. Query: NFC + straight quote.
        let nfd_curly: String = "Capture d\u{2019}écran.png".nfd().collect();
        std::fs::write(dir.path().join(&nfd_curly), b"img")?;
        let resolved = resolve_read_path("Capture d'écran.png", &cwd_str(dir.path()))?;
        assert_eq!(Path::new(&resolved), dir.path().join(&nfd_curly).as_path());
        Ok(())
    }

    #[test]
    fn resolve_read_path_prefers_earlier_variants() -> TestResult {
        let dir = tempfile::tempdir()?;
        // On disk: NFD form with a *regular* space before PM. Query in NFC:
        // the AM/PM variant (narrow no-break space) is checked first and
        // misses, then the NFD variant hits.
        let nfd_name: String = "café 1.2 PM.png".nfd().collect();
        std::fs::write(dir.path().join(&nfd_name), b"img")?;
        let resolved = resolve_read_path("café 1.2 PM.png", &cwd_str(dir.path()))?;
        assert_eq!(Path::new(&resolved), dir.path().join(&nfd_name).as_path());
        Ok(())
    }

    #[test]
    fn resolve_read_path_returns_resolved_when_nothing_exists() -> TestResult {
        let dir = tempfile::tempdir()?;
        let resolved = resolve_read_path("missing.txt", &cwd_str(dir.path()))?;
        assert_eq!(
            Path::new(&resolved),
            dir.path().join("missing.txt").as_path()
        );
        Ok(())
    }

    #[test]
    fn resolve_read_path_normalizes_unicode_space_input() -> TestResult {
        let dir = tempfile::tempdir()?;
        std::fs::write(dir.path().join("my file.txt"), b"data")?;
        let resolved = resolve_read_path("my\u{00A0}file.txt", &cwd_str(dir.path()))?;
        assert_eq!(
            Path::new(&resolved),
            dir.path().join("my file.txt").as_path()
        );
        Ok(())
    }

    #[test]
    fn macos_screenshot_variant_handles_case_and_repeats() {
        // Matches the `/ (AM|PM)\./gi` regex: any case, every occurrence.
        assert_eq!(try_macos_screenshot_path("a PM.png"), "a\u{202F}PM.png");
        assert_eq!(
            try_macos_screenshot_path("x am. y Am. z"),
            "x\u{202F}am. y\u{202F}Am. z"
        );
        // No trailing period, no match.
        assert_eq!(try_macos_screenshot_path("a PMx"), "a PMx");
        // Multi-byte neighbors are preserved.
        assert_eq!(try_macos_screenshot_path("é PM.png"), "é\u{202F}PM.png");
    }

    #[tokio::test]
    async fn resolve_read_path_async_matches_sync_behavior() -> TestResult {
        let dir = tempfile::tempdir()?;
        let curly_name = "Capture d\u{2019}écran.png";
        std::fs::write(dir.path().join(curly_name), b"img")?;
        let resolved = resolve_read_path_async("Capture d'écran.png", &cwd_str(dir.path())).await?;
        assert_eq!(Path::new(&resolved), dir.path().join(curly_name).as_path());

        let missing = resolve_read_path_async("missing.txt", &cwd_str(dir.path())).await?;
        assert_eq!(
            Path::new(&missing),
            dir.path().join("missing.txt").as_path()
        );

        assert!(path_exists(&cwd_str(dir.path())).await);
        assert!(!path_exists(&cwd_str(&dir.path().join("nope"))).await);
        Ok(())
    }
}
