//! Changelog extraction, link normalization, and startup decisions.

use std::{cmp::Ordering, fs, path::Path, sync::LazyLock};

use regex::{Captures, Regex};

const GITHUB_REPO: &str = "earendil-works/pi";
const BASE_PATH: &str = "packages/coding-agent";

static VERSION_HEADER: LazyLock<Option<Regex>> =
    LazyLock::new(|| Regex::new(r"^##\s+\[?(\d+)\.(\d+)\.(\d+)\]?").ok());
static MARKDOWN_LINK: LazyLock<Option<Regex>> =
    LazyLock::new(|| Regex::new(r"(!?\[[^\]\n]+\]\()([^\s)]+)((?:\s+[^)]*)?\))").ok());

/// One `## x.y.z` changelog section.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ChangelogEntry {
    /// Major version.
    pub major: u64,
    /// Minor version.
    pub minor: u64,
    /// Patch version.
    pub patch: u64,
    /// Header and section body.
    pub content: String,
}

impl ChangelogEntry {
    /// Dotted release version.
    #[must_use]
    pub fn version(&self) -> String {
        format!("{}.{}.{}", self.major, self.minor, self.patch)
    }
}

/// Parse all valid version sections. Missing and unreadable files produce no entries.
#[must_use]
pub fn parse_changelog(path: &Path) -> Vec<ChangelogEntry> {
    fs::read_to_string(path)
        .map(|content| parse_changelog_text(&content))
        .unwrap_or_default()
}

/// Parse changelog text without filesystem access.
#[must_use]
pub fn parse_changelog_text(content: &str) -> Vec<ChangelogEntry> {
    let Some(version_header) = VERSION_HEADER.as_ref() else {
        return Vec::new();
    };
    let mut entries = Vec::new();
    let mut current: Option<(u64, u64, u64, Vec<&str>)> = None;
    for line in content.split('\n') {
        if line.starts_with("## ") {
            push_entry(&mut entries, current.take());
            current = version_header.captures(line).and_then(|captures| {
                Some((
                    captures.get(1)?.as_str().parse().ok()?,
                    captures.get(2)?.as_str().parse().ok()?,
                    captures.get(3)?.as_str().parse().ok()?,
                    vec![line],
                ))
            });
        } else if let Some((_, _, _, lines)) = &mut current {
            lines.push(line);
        }
    }
    push_entry(&mut entries, current);
    entries
}

fn push_entry(entries: &mut Vec<ChangelogEntry>, current: Option<(u64, u64, u64, Vec<&str>)>) {
    if let Some((major, minor, patch, lines)) = current {
        entries.push(ChangelogEntry {
            major,
            minor,
            patch,
            content: lines.join("\n").trim().to_owned(),
        });
    }
}

/// Compare changelog versions.
#[must_use]
pub fn compare_versions(left: &ChangelogEntry, right: &ChangelogEntry) -> Ordering {
    (left.major, left.minor, left.patch).cmp(&(right.major, right.minor, right.patch))
}

/// Return entries newer than a stored dotted version.
#[must_use]
pub fn get_new_entries(entries: &[ChangelogEntry], last_version: &str) -> Vec<ChangelogEntry> {
    let mut parts = last_version
        .split('.')
        .map(|part| part.parse::<u64>().unwrap_or(0));
    let last = ChangelogEntry {
        major: parts.next().unwrap_or(0),
        minor: parts.next().unwrap_or(0),
        patch: parts.next().unwrap_or(0),
        content: String::new(),
    };
    entries
        .iter()
        .filter(|entry| compare_versions(entry, &last).is_gt())
        .cloned()
        .collect()
}

/// Normalize repository and relative links to a pinned release tag.
#[must_use]
pub fn normalize_changelog_links(markdown: &str, version: &str) -> String {
    let tag = if version.starts_with('v') {
        version.to_owned()
    } else {
        format!("v{version}")
    };
    let Some(markdown_link) = MARKDOWN_LINK.as_ref() else {
        return markdown.to_owned();
    };
    markdown_link
        .replace_all(markdown, |captures: &Captures<'_>| {
            let prefix = captures.get(1).map_or("", |value| value.as_str());
            let target = captures.get(2).map_or("", |value| value.as_str());
            let suffix = captures.get(3).map_or("", |value| value.as_str());
            format!("{prefix}{}{suffix}", normalize_target(target, &tag))
        })
        .into_owned()
}

fn normalize_target(target: &str, tag: &str) -> String {
    let mut canonical = target
        .replacen(
            "https://github.com/badlogic/pi-mono",
            "https://github.com/earendil-works/pi",
            1,
        )
        .replacen(
            "https://github.com/earendil-works/pi-mono",
            "https://github.com/earendil-works/pi",
            1,
        );
    let repo = format!("https://github.com/{GITHUB_REPO}");
    for route in ["blob", "tree"] {
        for branch in ["main", "master"] {
            let prefix = format!("{repo}/{route}/{branch}/");
            if let Some(rest) = canonical.strip_prefix(&prefix) {
                canonical = format!("{repo}/{route}/{tag}/{rest}");
            }
        }
    }
    if canonical.starts_with('#')
        || canonical.starts_with("//")
        || url::Url::parse(&canonical).is_ok()
    {
        return canonical;
    }
    let (path_query, fragment) = canonical
        .split_once('#')
        .map_or((canonical.as_str(), ""), |(path, fragment)| {
            (path, fragment)
        });
    let (path_part, query) = path_query
        .split_once('?')
        .map_or((path_query, ""), |(path, query)| (path, query));
    if path_part.is_empty() {
        return canonical;
    }
    let original_directory = path_part.ends_with('/');
    let mut segments: Vec<&str> = Vec::new();
    let normalized = path_part.replace('\\', "/");
    let absolute = normalized.starts_with('/');
    if !absolute {
        segments.extend(BASE_PATH.split('/'));
    }
    for segment in normalized.trim_start_matches('/').split('/') {
        match segment {
            "" | "." => {}
            ".." => {
                if segments.pop().is_none() {
                    return canonical;
                }
            }
            value => segments.push(value),
        }
    }
    if segments.is_empty() {
        return canonical;
    }
    let repository_path = segments.join("/");
    let basename = segments.last().copied().unwrap_or_default();
    let route = if original_directory || !basename.contains('.') {
        "tree"
    } else {
        "blob"
    };
    let query = if query.is_empty() {
        String::new()
    } else {
        format!("?{query}")
    };
    let fragment = if fragment.is_empty() {
        String::new()
    } else {
        format!("#{fragment}")
    };
    format!(
        "{repo}/{route}/{tag}/{}{query}{fragment}",
        encode_uri_path(&repository_path)
    )
}

/// Percent-encode a repository path the same way JavaScript `encodeURI` does.
///
/// `encodeURI` leaves unreserved and reserved URI characters intact and only
/// percent-encodes everything else (spaces, angle brackets, backticks, etc.).
/// This preserves the `/` separators within the path while encoding individual
/// segment characters that would break the URL.
fn encode_uri_path(path: &str) -> String {
    let mut out = String::with_capacity(path.len());
    for &byte in path.as_bytes() {
        match byte {
            b'A'..=b'Z'
            | b'a'..=b'z'
            | b'0'..=b'9'
            | b';'
            | b','
            | b'/'
            | b'?'
            | b':'
            | b'@'
            | b'&'
            | b'='
            | b'+'
            | b'$'
            | b'-'
            | b'_'
            | b'.'
            | b'!'
            | b'~'
            | b'*'
            | b'\''
            | b'('
            | b')'
            | b'#' => out.push(byte as char),
            _ => {
                use std::fmt::Write;
                out.push('%');
                write!(out, "{byte:02X}").unwrap_or_default();
            }
        }
    }
    out
}

/// Startup changelog state transition.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum StartupChangelogDecision {
    /// Resumed sessions do not interrupt the conversation.
    SkipResumed,
    /// First launch records the current version without showing history.
    RecordCurrent {
        /// Current product version to persist.
        version: String,
    },
    /// Show these entries and record the current version.
    Show {
        /// Current product version to persist.
        version: String,
        /// Entries newer than the last-recorded version.
        entries: Vec<ChangelogEntry>,
        /// Whether the display should start collapsed.
        collapsed: bool,
    },
    /// Nothing new; still record the current version.
    NoChanges {
        /// Current product version to persist.
        version: String,
    },
}

/// Decide whether startup should display the changelog.
#[must_use]
pub fn should_show_startup_changelog(
    session_has_messages: bool,
    last_version: Option<&str>,
    current_version: &str,
    entries: &[ChangelogEntry],
    collapsed: bool,
) -> StartupChangelogDecision {
    if session_has_messages {
        return StartupChangelogDecision::SkipResumed;
    }
    let Some(last_version) = last_version else {
        return StartupChangelogDecision::RecordCurrent {
            version: current_version.to_owned(),
        };
    };
    let new_entries = get_new_entries(entries, last_version);
    if new_entries.is_empty() {
        StartupChangelogDecision::NoChanges {
            version: current_version.to_owned(),
        }
    } else {
        StartupChangelogDecision::Show {
            version: current_version.to_owned(),
            entries: new_entries,
            collapsed,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_sections_and_ignores_non_version_h2() {
        let entries = parse_changelog_text(
            "# Log\n## [2.0.0] - now\nnew\n## Notes\nignored\n## 1.2.3\nold\n",
        );
        assert_eq!(entries.len(), 2);
        assert_eq!(entries[0].version(), "2.0.0");
        assert_eq!(entries[0].content, "## [2.0.0] - now\nnew");
        assert_eq!(get_new_entries(&entries, "1.9.0"), vec![entries[0].clone()]);
    }

    #[test]
    fn parse_changelog_text_handles_empty_and_no_version_headers() {
        assert!(parse_changelog_text("").is_empty());
        assert!(parse_changelog_text("# Title\nbody\n## Notes\nno version\n").is_empty());
    }

    #[test]
    fn parse_changelog_text_preserves_content_order_and_trailing_whitespace() {
        let entries = parse_changelog_text("## 1.0.0\nfirst\nline2\n## 2.0.0\nsecond");
        assert_eq!(entries.len(), 2);
        assert_eq!(entries[0].version(), "1.0.0");
        assert_eq!(entries[0].content, "## 1.0.0\nfirst\nline2");
        assert_eq!(entries[1].version(), "2.0.0");
        assert_eq!(entries[1].content, "## 2.0.0\nsecond");
    }

    #[test]
    fn parse_changelog_text_accepts_bracketed_and_plain_versions() {
        let entries = parse_changelog_text("## [1.0.0]\nbody1\n## 2.0.0\nbody2");
        assert_eq!(entries.len(), 2);
        assert_eq!(entries[0].major, 1);
        assert_eq!(entries[1].major, 2);
    }

    #[test]
    fn compare_versions_orders_correctly() {
        let v1 = ChangelogEntry {
            major: 1,
            minor: 0,
            patch: 0,
            content: String::new(),
        };
        let v2 = ChangelogEntry {
            major: 1,
            minor: 2,
            patch: 0,
            content: String::new(),
        };
        let v3 = ChangelogEntry {
            major: 1,
            minor: 2,
            patch: 5,
            content: String::new(),
        };
        let v4 = ChangelogEntry {
            major: 2,
            minor: 0,
            patch: 0,
            content: String::new(),
        };
        assert_eq!(compare_versions(&v1, &v2), Ordering::Less);
        assert_eq!(compare_versions(&v2, &v1), Ordering::Greater);
        assert_eq!(compare_versions(&v2, &v2), Ordering::Equal);
        assert_eq!(compare_versions(&v2, &v3), Ordering::Less);
        assert_eq!(compare_versions(&v3, &v4), Ordering::Less);
    }

    #[test]
    fn get_new_entries_filters_by_version_threshold() {
        let entries = parse_changelog_text("## 1.0.0\na\n## 1.5.0\nb\n## 2.0.0\nc");
        // All three are newer than 0.9.0.
        assert_eq!(get_new_entries(&entries, "0.9.0").len(), 3);
        // Only 2.0.0 is newer than 1.5.0.
        assert_eq!(get_new_entries(&entries, "1.5.0").len(), 1);
        // None are newer than 2.0.0.
        assert!(get_new_entries(&entries, "2.0.0").is_empty());
        // None are newer than a future version.
        assert!(get_new_entries(&entries, "3.0.0").is_empty());
    }

    #[test]
    fn get_new_entries_handles_malformed_last_version() {
        let entries = parse_changelog_text("## 1.0.0\na\n## 2.0.0\nb");
        // Malformed last version defaults to 0.0.0 -> all entries are "newer".
        assert_eq!(get_new_entries(&entries, "not-a-version").len(), 2);
        // Partial version string: "1" -> major=1, minor=0, patch=0.
        assert_eq!(get_new_entries(&entries, "1").len(), 1);
        // Empty string -> 0.0.0.
        assert_eq!(get_new_entries(&entries, "").len(), 2);
    }

    #[test]
    fn normalize_changelog_links_pins_floating_refs_to_tag() {
        let md = "[x](https://github.com/earendil-works/pi/blob/main/README.md)";
        let result = normalize_changelog_links(md, "1.2.3");
        assert!(result.contains("/blob/v1.2.3/README.md"));
        assert!(!result.contains("/blob/main/"));
    }

    #[test]
    fn normalize_changelog_links_remaps_master_branch() {
        let md = "[x](https://github.com/earendil-works/pi/tree/master/src)";
        let result = normalize_changelog_links(md, "1.0.0");
        assert!(result.contains("/tree/v1.0.0/src"));
    }

    #[test]
    fn normalize_changelog_links_rewrites_legacy_badlogic_repo() {
        let md = "[x](https://github.com/badlogic/pi-mono/blob/main/a.md)";
        let result = normalize_changelog_links(md, "1.0.0");
        assert!(result.contains("https://github.com/earendil-works/pi/blob/v1.0.0/a.md"));
    }

    #[test]
    fn normalize_changelog_links_rewrites_earendil_pi_mono_repo() {
        let md = "[x](https://github.com/earendil-works/pi-mono/blob/main/b.md)";
        let result = normalize_changelog_links(md, "1.0.0");
        assert!(result.contains("https://github.com/earendil-works/pi/blob/v1.0.0/b.md"));
    }

    #[test]
    fn normalize_changelog_links_preserves_external_urls() {
        let md = "[x](https://example.com/page) [y](http://foo.bar)";
        let result = normalize_changelog_links(md, "1.0.0");
        assert!(result.contains("https://example.com/page"));
        assert!(result.contains("http://foo.bar"));
    }

    #[test]
    fn normalize_changelog_links_preserves_fragments_and_query_strings() {
        let md = "[x](docs/page.md#section) [y](api?q=1)";
        let result = normalize_changelog_links(md, "1.0.0");
        assert!(result.contains("#section"));
        assert!(result.contains("?q=1"));
    }

    #[test]
    fn normalize_changelog_links_uses_tree_route_for_directories() {
        let md = "[x](src/dir/)";
        let result = normalize_changelog_links(md, "1.0.0");
        assert!(result.contains("/tree/v1.0.0/"));
    }

    #[test]
    fn normalize_changelog_links_uses_blob_route_for_files() {
        let md = "[x](docs/readme.md)";
        let result = normalize_changelog_links(md, "1.0.0");
        assert!(result.contains("/blob/v1.0.0/"));
    }

    #[test]
    fn normalize_changelog_links_prepends_base_path_for_relative_links() {
        let md = "[x](utils/helper.ts)";
        let result = normalize_changelog_links(md, "1.0.0");
        assert!(result.contains("/blob/v1.0.0/packages/coding-agent/utils/helper.ts"));
    }

    #[test]
    fn normalize_changelog_links_handles_absolute_paths() {
        let md = "[x](/absolute/file.ts)";
        let result = normalize_changelog_links(md, "1.0.0");
        // Absolute paths do not get the base path prepended.
        assert!(result.contains("/blob/v1.0.0/absolute/file.ts"));
        assert!(!result.contains("packages/coding-agent/absolute"));
    }

    #[test]
    fn normalize_changelog_links_accepts_v_prefixed_version() {
        let md = "[x](docs/a.md)";
        let result = normalize_changelog_links(md, "v3.0.0");
        assert!(result.contains("/blob/v3.0.0/"));
    }

    #[test]
    fn normalize_changelog_links_preserves_image_links() {
        let md = "![alt](image.png)";
        let result = normalize_changelog_links(md, "1.0.0");
        assert!(result.starts_with("![alt]("));
        assert!(result.contains("/blob/v1.0.0/packages/coding-agent/image.png"));
    }

    #[test]
    fn normalize_changelog_links_preserves_hash_only_links() {
        let md = "[x](#section)";
        let result = normalize_changelog_links(md, "1.0.0");
        assert_eq!(result, "[x](#section)");
    }

    #[test]
    fn encode_uri_path_percent_encodes_spaces() {
        assert_eq!(encode_uri_path("docs/my file.md"), "docs/my%20file.md");
    }

    #[test]
    fn encode_uri_path_percent_encodes_special_characters() {
        assert_eq!(encode_uri_path("docs/<angle>.md"), "docs/%3Cangle%3E.md");
    }

    #[test]
    fn encode_uri_path_preserves_unreserved_characters() {
        // Characters that encodeURI leaves alone: - _ . ! ~ * ' ( )
        let path = "docs/file_v1.0!~'*().md";
        assert_eq!(encode_uri_path(path), path);
    }

    #[test]
    fn encode_uri_path_preserves_slash_separators() {
        assert_eq!(encode_uri_path("sub dir/file.md"), "sub%20dir/file.md");
    }

    #[test]
    fn normalize_changelog_links_encodes_unicode_in_paths() {
        let md = "[x](docs/caf\u{00e9}.md)";
        let result = normalize_changelog_links(md, "1.0.0");
        // UTF-8 bytes of é are 0xC3 0xA9.
        assert!(result.contains("caf%C3%A9.md"));
    }

    #[test]
    fn startup_decision_skips_resumed_sessions() {
        let entries = parse_changelog_text("## 2.0.0\nnew");
        let decision = should_show_startup_changelog(true, Some("1.0.0"), "2.0.0", &entries, false);
        assert!(matches!(decision, StartupChangelogDecision::SkipResumed));
    }

    #[test]
    fn startup_decision_shows_when_new_entries_exist() {
        let entries = parse_changelog_text("## 1.5.0\na\n## 2.0.0\nb");
        let decision = should_show_startup_changelog(false, Some("1.0.0"), "2.0.0", &entries, true);
        assert!(matches!(decision, StartupChangelogDecision::Show { .. }));
        if let StartupChangelogDecision::Show {
            version,
            entries,
            collapsed,
        } = decision
        {
            assert_eq!(version, "2.0.0");
            assert_eq!(entries.len(), 2);
            assert!(collapsed);
        }
    }

    #[test]
    fn startup_decision_records_current_on_first_launch() {
        let entries = parse_changelog_text("## 2.0.0\nnew");
        let decision = should_show_startup_changelog(false, None, "2.0.0", &entries, false);
        assert!(matches!(
            decision,
            StartupChangelogDecision::RecordCurrent { .. }
        ));
        if let StartupChangelogDecision::RecordCurrent { version } = decision {
            assert_eq!(version, "2.0.0");
        }
    }

    #[test]
    fn startup_decision_no_changes_when_up_to_date() {
        let entries = parse_changelog_text("## 2.0.0\nnew");
        let decision =
            should_show_startup_changelog(false, Some("2.0.0"), "2.0.0", &entries, false);
        assert!(matches!(
            decision,
            StartupChangelogDecision::NoChanges { .. }
        ));
        if let StartupChangelogDecision::NoChanges { version } = decision {
            assert_eq!(version, "2.0.0");
        }
    }
}
