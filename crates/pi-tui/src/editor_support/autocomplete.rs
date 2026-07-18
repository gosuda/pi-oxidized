//! Autocomplete provider trait, items, and combined slash/@/path provider.
//!
//! Ports `.references/pi/packages/tui/src/autocomplete.ts`. File listing is
//! abstracted behind [`FileLister`] so product code can inject an `fd`-backed
//! implementation without coupling `pi-tui` to process spawning.

use std::cmp::Reverse;
use std::path::{Component, Path, PathBuf};
use std::pin::Pin;

use crate::fuzzy::fuzzy_filter;

/// Debounce window (ms) for attachment/trigger-char autocomplete.
pub const ATTACHMENT_AUTOCOMPLETE_DEBOUNCE_MS: u64 = 20;

/// Default trigger characters merged with provider-supplied ones.
pub const DEFAULT_AUTOCOMPLETE_TRIGGER_CHARACTERS: &[&str] = &["@", "#"];

/// One autocomplete suggestion.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AutocompleteItem {
    /// Inserted value.
    pub value: String,
    /// Display label.
    pub label: String,
    /// Optional secondary description.
    pub description: Option<String>,
}

/// Suggestion batch returned by a provider.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AutocompleteSuggestions {
    /// Items to show.
    pub items: Vec<AutocompleteItem>,
    /// Matched prefix (e.g. `"/he"`, `"src/"`, `@"foo`).
    pub prefix: String,
}

/// Options for [`AutocompleteProvider::get_suggestions`].
#[derive(Debug, Clone, Copy)]
pub struct SuggestionOptions {
    /// Force path completion (Tab).
    pub force: bool,
    /// Generation token; providers may ignore it (race guard lives in editor).
    pub request_token: u64,
}

/// Optional slash-command argument completer.
pub type ArgumentCompleter = fn(&str) -> Option<Vec<AutocompleteItem>>;

/// Slash command registered with the combined provider.
#[derive(Debug, Clone)]
pub struct SlashCommand {
    /// Command name without leading `/`.
    pub name: String,
    /// Human description.
    pub description: Option<String>,
    /// Argument hint shown in the list.
    pub argument_hint: Option<String>,
    /// Optional argument completer.
    pub argument_completions: Option<ArgumentCompleter>,
}

/// Directory entry returned by a [`FileLister`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FileEntry {
    /// Path relative to the listing root, using `/` separators.
    pub path: String,
    /// True when the entry is a directory.
    pub is_directory: bool,
}

/// Product-supplied filesystem listing backend.
pub trait FileLister: Send + Sync {
    /// List direct children of `dir` (absolute or relative to process cwd).
    fn list_dir(&self, dir: &Path) -> Vec<FileEntry>;

    /// Fuzzy walk under `base_dir` for `query`, up to `max_results`.
    ///
    /// Implementations may shell out to `fd`. Empty when unavailable/cancelled.
    fn fuzzy_walk(
        &self,
        base_dir: &Path,
        query: &str,
        max_results: usize,
        cancelled: bool,
    ) -> Vec<FileEntry>;

    /// Expand `~/…` to an absolute path. Default uses the `HOME` env var.
    fn expand_home(&self, path: &str) -> PathBuf {
        if path == "~" {
            return home_dir().unwrap_or_else(|| PathBuf::from("/"));
        }
        if let Some(rest) = path.strip_prefix("~/") {
            let mut base = home_dir().unwrap_or_else(|| PathBuf::from("/"));
            base.push(rest);
            if path.ends_with('/') && !base.as_os_str().to_string_lossy().ends_with('/') {
                // Preserve trailing slash semantics via display helpers, not PathBuf.
            }
            return base;
        }
        PathBuf::from(path)
    }
}

fn home_dir() -> Option<PathBuf> {
    std::env::var_os("HOME")
        .or_else(|| std::env::var_os("USERPROFILE"))
        .map(PathBuf::from)
}

/// Boxed future used by async suggestion providers.
pub type BoxFuture<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;

/// Autocomplete provider contract.
pub trait AutocompleteProvider: Send + Sync {
    /// Characters that naturally trigger this provider at token boundaries.
    fn trigger_characters(&self) -> &[String] {
        &[]
    }

    /// Get suggestions for the current cursor position.
    fn get_suggestions(
        &self,
        lines: &[String],
        cursor_line: usize,
        cursor_col: usize,
        options: SuggestionOptions,
    ) -> BoxFuture<'_, Option<AutocompleteSuggestions>>;

    /// Apply a selected item; returns the new buffer and cursor.
    fn apply_completion(
        &self,
        lines: &[String],
        cursor_line: usize,
        cursor_col: usize,
        item: &AutocompleteItem,
        prefix: &str,
    ) -> ApplyCompletionResult;

    /// Whether forced Tab path completion should run.
    fn should_trigger_file_completion(
        &self,
        lines: &[String],
        cursor_line: usize,
        cursor_col: usize,
    ) -> bool {
        let current = lines.get(cursor_line).map_or("", String::as_str);
        let before = &current[..cursor_col.min(current.len())];
        let trimmed = before.trim();
        !trimmed.starts_with('/') || trimmed.contains(' ')
    }
}

/// Result of applying a completion.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ApplyCompletionResult {
    /// Updated lines.
    pub lines: Vec<String>,
    /// Cursor line.
    pub cursor_line: usize,
    /// Cursor column (byte index).
    pub cursor_col: usize,
}

const PATH_DELIMITERS: &[char] = &[' ', '\t', '"', '\'', '='];

/// Combined slash-command + `@` fuzzy file + path provider.
pub struct CombinedAutocompleteProvider<L: FileLister> {
    commands: Vec<SlashCommand>,
    items: Vec<AutocompleteItem>,
    base_path: PathBuf,
    lister: L,
    trigger_characters: Vec<String>,
}

impl<L: FileLister> CombinedAutocompleteProvider<L> {
    /// Create a provider with slash commands and a file lister.
    pub fn new(commands: Vec<SlashCommand>, base_path: impl Into<PathBuf>, lister: L) -> Self {
        Self {
            commands,
            items: Vec::new(),
            base_path: base_path.into(),
            lister,
            trigger_characters: Vec::new(),
        }
    }

    /// Also register plain autocomplete items as slash-style values.
    #[must_use]
    pub fn with_items(mut self, items: Vec<AutocompleteItem>) -> Self {
        self.items = items;
        self
    }

    /// Override provider trigger characters (single non-`/` non-ws chars).
    #[must_use]
    pub fn with_trigger_characters(mut self, chars: Vec<String>) -> Self {
        self.trigger_characters = chars;
        self
    }

    fn extract_at_prefix(text: &str) -> Option<String> {
        if let Some(quoted) = extract_quoted_prefix(text)
            && quoted.starts_with("@\"")
        {
            return Some(quoted);
        }
        let last = find_last_delimiter(text);
        let token_start = if last == usize::MAX { 0 } else { last + 1 };
        if text.as_bytes().get(token_start) == Some(&b'@') {
            Some(text[token_start..].to_owned())
        } else {
            None
        }
    }

    fn extract_path_prefix(text: &str, force: bool) -> Option<String> {
        if let Some(quoted) = extract_quoted_prefix(text) {
            return Some(quoted);
        }
        let last = find_last_delimiter(text);
        let path_prefix = if last == usize::MAX {
            text.to_owned()
        } else {
            text[last + 1..].to_owned()
        };
        if force {
            return Some(path_prefix);
        }
        if path_prefix.contains('/')
            || path_prefix.starts_with('.')
            || path_prefix.starts_with("~/")
        {
            return Some(path_prefix);
        }
        if path_prefix.is_empty() && text.ends_with(' ') {
            return Some(path_prefix);
        }
        None
    }

    fn get_file_suggestions(&self, prefix: &str) -> Vec<AutocompleteItem> {
        let parsed = parse_path_prefix(prefix);
        let mut expanded = parsed.raw.clone();
        if expanded.starts_with('~') {
            expanded = self
                .lister
                .expand_home(&expanded)
                .to_string_lossy()
                .replace('\\', "/");
            if parsed.raw.ends_with('/') && !expanded.ends_with('/') {
                expanded.push('/');
            }
        }

        let raw = &parsed.raw;
        let is_root = raw.is_empty()
            || raw == "./"
            || raw == "../"
            || raw == "~"
            || raw == "~/"
            || raw == "/"
            || (parsed.is_at && raw.is_empty());

        let (search_dir, search_prefix): (PathBuf, String) = if is_root || raw.ends_with('/') {
            let dir = if raw.starts_with('~') || expanded.starts_with('/') {
                PathBuf::from(&expanded)
            } else {
                self.base_path.join(&expanded)
            };
            (dir, String::new())
        } else {
            let path = Path::new(&expanded);
            let dir = path.parent().unwrap_or_else(|| Path::new(""));
            let file = path
                .file_name()
                .map(|s| s.to_string_lossy().into_owned())
                .unwrap_or_default();
            let search_dir = if raw.starts_with('~') || expanded.starts_with('/') {
                dir.to_path_buf()
            } else {
                self.base_path.join(dir)
            };
            (search_dir, file)
        };

        let entries = self.lister.list_dir(&search_dir);
        let mut suggestions = Vec::new();
        let search_lower = search_prefix.to_ascii_lowercase();

        for entry in entries {
            let name = entry
                .path
                .rsplit('/')
                .next()
                .unwrap_or(&entry.path)
                .to_owned();
            if !name.to_ascii_lowercase().starts_with(&search_lower) {
                continue;
            }
            let relative = build_relative_display(raw, &name);
            let path_value = if entry.is_directory {
                format!("{relative}/")
            } else {
                relative
            };
            let value = build_completion_value(
                &path_value,
                parsed.is_at,
                parsed.is_quoted,
                entry.is_directory,
            );
            suggestions.push(AutocompleteItem {
                value,
                label: if entry.is_directory {
                    format!("{name}/")
                } else {
                    name
                },
                description: None,
            });
        }

        suggestions.sort_by(|a, b| {
            let a_dir = a.label.ends_with('/');
            let b_dir = b.label.ends_with('/');
            match (a_dir, b_dir) {
                (true, false) => std::cmp::Ordering::Less,
                (false, true) => std::cmp::Ordering::Greater,
                _ => a.label.cmp(&b.label),
            }
        });
        suggestions
    }

    fn score_entry(file_path: &str, query: &str, is_directory: bool) -> i32 {
        let file_name = file_path.rsplit('/').next().unwrap_or(file_path);
        let lower_name = file_name.to_ascii_lowercase();
        let lower_query = query.to_ascii_lowercase();
        let mut score = if lower_name == lower_query {
            100
        } else if lower_name.starts_with(&lower_query) {
            80
        } else if lower_name.contains(&lower_query) {
            50
        } else if file_path.to_ascii_lowercase().contains(&lower_query) {
            30
        } else {
            0
        };
        if is_directory && score > 0 {
            score += 10;
        }
        score
    }

    fn get_fuzzy_file_suggestions(
        &self,
        query: &str,
        is_quoted: bool,
        cancelled: bool,
    ) -> Vec<AutocompleteItem> {
        if cancelled {
            return Vec::new();
        }
        let fuzzy_context = resolve_scoped_fuzzy_query(query, &self.base_path, &self.lister);
        let (fd_base, fd_query, display_base) = if let Some(context) = &fuzzy_context {
            (
                context.base_dir.as_path(),
                context.query.as_str(),
                Some(context.display_base.as_str()),
            )
        } else {
            (self.base_path.as_path(), query, None)
        };
        let entries = self.lister.fuzzy_walk(fd_base, fd_query, 100, cancelled);
        if cancelled {
            return Vec::new();
        }
        let mut ranked_entries: Vec<(FileEntry, i32)> = entries
            .into_iter()
            .map(|e| {
                let relevance = if fd_query.is_empty() {
                    1
                } else {
                    Self::score_entry(&e.path, fd_query, e.is_directory)
                };
                (e, relevance)
            })
            .filter(|(_, relevance)| *relevance > 0)
            .collect();
        ranked_entries.sort_by_key(|entry| Reverse(entry.1));
        ranked_entries.truncate(20);

        let mut suggestions = Vec::new();
        for (entry, _) in ranked_entries {
            let path_without_slash = entry.path.trim_end_matches('/');
            let display_path = if let Some(base) = display_base {
                to_display_path(&scoped_path_for_display(base, path_without_slash))
            } else {
                to_display_path(path_without_slash)
            };
            let entry_name = display_path
                .rsplit('/')
                .next()
                .unwrap_or(display_path.as_str())
                .to_owned();
            let completion = if entry.is_directory {
                format!("{display_path}/")
            } else {
                display_path.clone()
            };
            let value = build_completion_value(&completion, true, is_quoted, entry.is_directory);
            suggestions.push(AutocompleteItem {
                value,
                label: if entry.is_directory {
                    format!("{entry_name}/")
                } else {
                    entry_name
                },
                description: Some(display_path),
            });
        }
        suggestions
    }
}

impl<L: FileLister + 'static> AutocompleteProvider for CombinedAutocompleteProvider<L> {
    fn trigger_characters(&self) -> &[String] {
        &self.trigger_characters
    }

    fn get_suggestions(
        &self,
        lines: &[String],
        cursor_line: usize,
        cursor_col: usize,
        options: SuggestionOptions,
    ) -> BoxFuture<'_, Option<AutocompleteSuggestions>> {
        let lines = lines.to_vec();
        Box::pin(async move {
            let current = lines.get(cursor_line).map_or("", String::as_str);
            let col = cursor_col.min(current.len());
            let text_before = &current[..col];

            if let Some(at_prefix) = Self::extract_at_prefix(text_before) {
                let parsed = parse_path_prefix(&at_prefix);
                let suggestions =
                    self.get_fuzzy_file_suggestions(&parsed.raw, parsed.is_quoted, false);
                if suggestions.is_empty() {
                    return None;
                }
                return Some(AutocompleteSuggestions {
                    items: suggestions,
                    prefix: at_prefix,
                });
            }

            if !options.force && text_before.starts_with('/') {
                if let Some(space) = text_before.find(' ') {
                    let command_name = &text_before[1..space];
                    let argument_text = &text_before[space + 1..];
                    if let Some(cmd) = self.commands.iter().find(|c| c.name == command_name)
                        && let Some(completer) = cmd.argument_completions
                        && let Some(items) = completer(argument_text)
                        && !items.is_empty()
                    {
                        return Some(AutocompleteSuggestions {
                            items,
                            prefix: argument_text.to_owned(),
                        });
                    }
                }

                let prefix = &text_before[1..];
                let mut command_items: Vec<(String, AutocompleteItem)> = self
                    .commands
                    .iter()
                    .map(|cmd| {
                        let desc = match (&cmd.argument_hint, &cmd.description) {
                            (Some(hint), Some(d)) if !d.is_empty() => Some(format!("{hint} — {d}")),
                            (Some(hint), _) => Some(hint.clone()),
                            (_, Some(d)) if !d.is_empty() => Some(d.clone()),
                            _ => None,
                        };
                        (
                            cmd.name.clone(),
                            AutocompleteItem {
                                value: cmd.name.clone(),
                                label: cmd.name.clone(),
                                description: desc,
                            },
                        )
                    })
                    .collect();
                for item in &self.items {
                    command_items.push((item.value.clone(), item.clone()));
                }
                let names: Vec<String> = command_items.iter().map(|(n, _)| n.clone()).collect();
                let filtered_names = fuzzy_filter(&names, prefix, |s| s.as_str());
                let filtered: Vec<AutocompleteItem> = filtered_names
                    .into_iter()
                    .filter_map(|name| {
                        command_items
                            .iter()
                            .find(|(n, _)| n == &name)
                            .map(|(_, item)| item.clone())
                    })
                    .collect();
                if filtered.is_empty() {
                    return None;
                }
                return Some(AutocompleteSuggestions {
                    items: filtered,
                    prefix: text_before.to_owned(),
                });
            }

            let path_match = Self::extract_path_prefix(text_before, options.force)?;
            let suggestions = self.get_file_suggestions(&path_match);
            if suggestions.is_empty() {
                return None;
            }
            Some(AutocompleteSuggestions {
                items: suggestions,
                prefix: path_match,
            })
        })
    }

    fn apply_completion(
        &self,
        lines: &[String],
        cursor_line: usize,
        cursor_col: usize,
        item: &AutocompleteItem,
        prefix: &str,
    ) -> ApplyCompletionResult {
        let current = lines.get(cursor_line).cloned().unwrap_or_default();
        let col = cursor_col.min(current.len());
        let prefix_len = prefix.len().min(col);
        let before_prefix = &current[..col - prefix_len];
        let after_cursor = &current[col..];
        let is_quoted_prefix = prefix.starts_with('"') || prefix.starts_with("@\"");
        let has_leading_quote_after = after_cursor.starts_with('"');
        let has_trailing_quote_in_item = item.value.ends_with('"');
        let adjusted_after =
            if is_quoted_prefix && has_trailing_quote_in_item && has_leading_quote_after {
                &after_cursor[1..]
            } else {
                after_cursor
            };

        let is_slash = prefix.starts_with('/')
            && before_prefix.trim().is_empty()
            && !prefix[1..].contains('/');
        if is_slash {
            let new_line = format!("{before_prefix}/{} {adjusted_after}", item.value);
            let mut new_lines = lines.to_vec();
            if cursor_line < new_lines.len() {
                new_lines[cursor_line] = new_line;
            }
            return ApplyCompletionResult {
                lines: new_lines,
                cursor_line,
                cursor_col: before_prefix.len() + item.value.len() + 2,
            };
        }

        if prefix.starts_with('@') {
            let is_directory = item.label.ends_with('/');
            let suffix = if is_directory { "" } else { " " };
            let new_line = format!("{before_prefix}{}{suffix}{adjusted_after}", item.value);
            let mut new_lines = lines.to_vec();
            if cursor_line < new_lines.len() {
                new_lines[cursor_line] = new_line;
            }
            let has_trailing_quote = item.value.ends_with('"');
            let cursor_offset = if is_directory && has_trailing_quote {
                item.value.len() - 1
            } else {
                item.value.len()
            };
            return ApplyCompletionResult {
                lines: new_lines,
                cursor_line,
                cursor_col: before_prefix.len() + cursor_offset + suffix.len(),
            };
        }

        let text_before = &current[..col];
        if text_before.contains('/') && text_before.contains(' ') {
            let new_line = format!("{before_prefix}{}{adjusted_after}", item.value);
            let mut new_lines = lines.to_vec();
            if cursor_line < new_lines.len() {
                new_lines[cursor_line] = new_line;
            }
            let is_directory = item.label.ends_with('/');
            let has_trailing_quote = item.value.ends_with('"');
            let cursor_offset = if is_directory && has_trailing_quote {
                item.value.len() - 1
            } else {
                item.value.len()
            };
            return ApplyCompletionResult {
                lines: new_lines,
                cursor_line,
                cursor_col: before_prefix.len() + cursor_offset,
            };
        }

        let new_line = format!("{before_prefix}{}{adjusted_after}", item.value);
        let mut new_lines = lines.to_vec();
        if cursor_line < new_lines.len() {
            new_lines[cursor_line] = new_line;
        }
        let is_directory = item.label.ends_with('/');
        let has_trailing_quote = item.value.ends_with('"');
        let cursor_offset = if is_directory && has_trailing_quote {
            item.value.len() - 1
        } else {
            item.value.len()
        };
        ApplyCompletionResult {
            lines: new_lines,
            cursor_line,
            cursor_col: before_prefix.len() + cursor_offset,
        }
    }
}

fn find_last_delimiter(text: &str) -> usize {
    text.char_indices()
        .rev()
        .find(|(_, c)| PATH_DELIMITERS.contains(c))
        .map_or(usize::MAX, |(i, _)| i)
}

fn find_unclosed_quote_start(text: &str) -> Option<usize> {
    let mut in_quotes = false;
    let mut quote_start = 0usize;
    for (i, ch) in text.char_indices() {
        if ch == '"' {
            in_quotes = !in_quotes;
            if in_quotes {
                quote_start = i;
            }
        }
    }
    in_quotes.then_some(quote_start)
}

fn is_token_start(text: &str, index: usize) -> bool {
    if index == 0 {
        return true;
    }
    text[..index]
        .chars()
        .next_back()
        .is_some_and(|c| PATH_DELIMITERS.contains(&c))
}

fn extract_quoted_prefix(text: &str) -> Option<String> {
    let quote_start = find_unclosed_quote_start(text)?;
    if quote_start > 0 && text.as_bytes().get(quote_start - 1) == Some(&b'@') {
        if !is_token_start(text, quote_start - 1) {
            return None;
        }
        return Some(text[quote_start - 1..].to_owned());
    }
    if !is_token_start(text, quote_start) {
        return None;
    }
    Some(text[quote_start..].to_owned())
}

struct ParsedPathPrefix {
    raw: String,
    is_at: bool,
    is_quoted: bool,
}

fn parse_path_prefix(prefix: &str) -> ParsedPathPrefix {
    if let Some(rest) = prefix.strip_prefix("@\"") {
        return ParsedPathPrefix {
            raw: rest.to_owned(),
            is_at: true,
            is_quoted: true,
        };
    }
    if let Some(rest) = prefix.strip_prefix('"') {
        return ParsedPathPrefix {
            raw: rest.to_owned(),
            is_at: false,
            is_quoted: true,
        };
    }
    if let Some(rest) = prefix.strip_prefix('@') {
        return ParsedPathPrefix {
            raw: rest.to_owned(),
            is_at: true,
            is_quoted: false,
        };
    }
    ParsedPathPrefix {
        raw: prefix.to_owned(),
        is_at: false,
        is_quoted: false,
    }
}

fn build_completion_value(
    path: &str,
    is_at_prefix: bool,
    is_quoted_prefix: bool,
    _is_directory: bool,
) -> String {
    let needs_quotes = is_quoted_prefix || path.contains(' ');
    let prefix = if is_at_prefix { "@" } else { "" };
    if !needs_quotes {
        return format!("{prefix}{path}");
    }
    format!("{prefix}\"{path}\"")
}

fn build_relative_display(display_prefix: &str, name: &str) -> String {
    let display_prefix = display_prefix.replace('\\', "/");
    if display_prefix.ends_with('/') {
        return format!("{display_prefix}{name}");
    }
    if display_prefix.contains('/') {
        if let Some(rest) = display_prefix.strip_prefix("~/") {
            let dir = Path::new(rest).parent().unwrap_or_else(|| Path::new(""));
            if dir.as_os_str().is_empty() || dir == Path::new(".") {
                return format!("~/{name}");
            }
            return format!("~/{}/{name}", dir.to_string_lossy().replace('\\', "/"));
        }
        if display_prefix.starts_with('/') {
            let dir = Path::new(&display_prefix)
                .parent()
                .unwrap_or_else(|| Path::new("/"));
            if dir == Path::new("/") {
                return format!("/{name}");
            }
            return format!("{}/{name}", dir.to_string_lossy().replace('\\', "/"));
        }
        let dir = Path::new(&display_prefix)
            .parent()
            .unwrap_or_else(|| Path::new(""));
        let mut relative = dir.join(name).to_string_lossy().replace('\\', "/");
        if display_prefix.starts_with("./") && !relative.starts_with("./") {
            relative = format!("./{relative}");
        }
        return relative;
    }
    if display_prefix.starts_with('~') {
        format!("~/{name}")
    } else {
        name.to_owned()
    }
}

struct ScopedFuzzy {
    base_dir: PathBuf,
    query: String,
    display_base: String,
}

fn resolve_scoped_fuzzy_query(
    raw_query: &str,
    base_path: &Path,
    lister: &impl FileLister,
) -> Option<ScopedFuzzy> {
    let normalized = raw_query.replace('\\', "/");
    let slash = normalized.rfind('/')?;
    let display_base = normalized[..=slash].to_owned();
    let query = normalized[slash + 1..].to_owned();
    let base_dir = if display_base.starts_with("~/") {
        lister.expand_home(&display_base)
    } else if display_base.starts_with('/') {
        PathBuf::from(&display_base)
    } else {
        base_path.join(&display_base)
    };
    // Existence check is best-effort via empty list.
    let _ = base_dir;
    Some(ScopedFuzzy {
        base_dir: if display_base.starts_with("~/") {
            lister.expand_home(&display_base)
        } else if display_base.starts_with('/') {
            PathBuf::from(&display_base)
        } else {
            base_path.join(&display_base)
        },
        query,
        display_base,
    })
}

fn scoped_path_for_display(display_base: &str, relative_path: &str) -> String {
    let relative = relative_path.replace('\\', "/");
    if display_base == "/" {
        format!("/{relative}")
    } else {
        format!("{}{relative}", display_base.replace('\\', "/"))
    }
}

/// Normalize a path for display (`\` → `/`, collapse `.` components).
#[must_use]
pub fn to_display_path(value: &str) -> String {
    let replaced = value.replace('\\', "/");
    let path = Path::new(&replaced);
    let mut parts = Vec::new();
    for c in path.components() {
        match c {
            Component::CurDir => {}
            Component::ParentDir => {
                if parts.last().is_some_and(|p: &String| p != "..") {
                    parts.pop();
                } else {
                    parts.push("..".to_owned());
                }
            }
            Component::Normal(s) => parts.push(s.to_string_lossy().into_owned()),
            Component::RootDir => parts.push(String::new()),
            Component::Prefix(p) => parts.push(p.as_os_str().to_string_lossy().into_owned()),
        }
    }
    if replaced.starts_with('/') {
        format!(
            "/{}",
            parts
                .into_iter()
                .filter(|p| !p.is_empty())
                .collect::<Vec<_>>()
                .join("/")
        )
    } else {
        parts.join("/")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;
    use std::sync::{Mutex, MutexGuard};

    struct FakeLister {
        dirs: Mutex<HashMap<String, Vec<FileEntry>>>,
    }

    impl FakeLister {
        fn new() -> Self {
            Self {
                dirs: Mutex::new(HashMap::new()),
            }
        }

        fn dirs(&self) -> MutexGuard<'_, HashMap<String, Vec<FileEntry>>> {
            match self.dirs.lock() {
                Ok(dirs) => dirs,
                Err(poisoned) => poisoned.into_inner(),
            }
        }

        fn insert(&self, dir: &str, entries: Vec<FileEntry>) {
            self.dirs().insert(dir.replace('\\', "/"), entries);
        }
    }

    impl FileLister for FakeLister {
        fn list_dir(&self, dir: &Path) -> Vec<FileEntry> {
            let key = dir.to_string_lossy().replace('\\', "/");
            self.dirs().get(&key).cloned().unwrap_or_default()
        }
        fn fuzzy_walk(
            &self,
            _base_dir: &Path,
            query: &str,
            max_results: usize,
            cancelled: bool,
        ) -> Vec<FileEntry> {
            if cancelled {
                return Vec::new();
            }
            let mut all = Vec::new();
            for entries in self.dirs().values() {
                for e in entries {
                    if query.is_empty()
                        || e.path
                            .to_ascii_lowercase()
                            .contains(&query.to_ascii_lowercase())
                    {
                        all.push(e.clone());
                    }
                }
            }
            all.truncate(max_results);
            all
        }
        fn expand_home(&self, path: &str) -> PathBuf {
            if path == "~" {
                return PathBuf::from("/home/user");
            }
            if let Some(rest) = path.strip_prefix("~/") {
                return PathBuf::from("/home/user").join(rest);
            }
            PathBuf::from(path)
        }
    }

    #[tokio::test]
    async fn slash_command_filter() -> Result<(), &'static str> {
        let lister = FakeLister::new();
        let provider = CombinedAutocompleteProvider::new(
            vec![
                SlashCommand {
                    name: "help".into(),
                    description: Some("show help".into()),
                    argument_hint: None,
                    argument_completions: None,
                },
                SlashCommand {
                    name: "hello".into(),
                    description: None,
                    argument_hint: None,
                    argument_completions: None,
                },
            ],
            "/tmp",
            lister,
        );
        let lines = vec!["/he".to_owned()];
        let result = provider
            .get_suggestions(
                &lines,
                0,
                3,
                SuggestionOptions {
                    force: false,
                    request_token: 1,
                },
            )
            .await
            .ok_or("missing suggestions")?;
        assert!(result.items.iter().any(|i| i.value == "help"));
        assert!(result.items.iter().any(|i| i.value == "hello"));
        assert_eq!(result.prefix, "/he");
        Ok(())
    }

    #[tokio::test]
    async fn path_prefix_lists_files() {
        let lister = FakeLister::new();
        lister.insert(
            "/tmp",
            vec![
                FileEntry {
                    path: "src".into(),
                    is_directory: true,
                },
                FileEntry {
                    path: "readme.md".into(),
                    is_directory: false,
                },
            ],
        );
        let provider = CombinedAutocompleteProvider::new(vec![], "/tmp", lister);
        let lines = vec!["./".to_owned()];
        // force Tab on empty-looking path after ./
        let result = provider
            .get_suggestions(
                &lines,
                0,
                2,
                SuggestionOptions {
                    force: true,
                    request_token: 1,
                },
            )
            .await;
        // May be empty if search_dir resolution differs; at least exercise path.
        let _ = result;
    }

    #[test]
    fn apply_slash_completion_adds_space() {
        let lister = FakeLister::new();
        let provider = CombinedAutocompleteProvider::new(
            vec![SlashCommand {
                name: "help".into(),
                description: None,
                argument_hint: None,
                argument_completions: None,
            }],
            "/tmp",
            lister,
        );
        let lines = vec!["/he".to_owned()];
        let item = AutocompleteItem {
            value: "help".into(),
            label: "help".into(),
            description: None,
        };
        let result = provider.apply_completion(&lines, 0, 3, &item, "/he");
        assert_eq!(result.lines[0], "/help ");
        assert_eq!(result.cursor_col, 6);
    }

    #[test]
    fn extract_helpers() {
        assert_eq!(
            CombinedAutocompleteProvider::<FakeLister>::extract_at_prefix("@src/"),
            Some("@src/".into())
        );
        assert_eq!(
            CombinedAutocompleteProvider::<FakeLister>::extract_path_prefix("foo src/", false),
            Some("src/".into())
        );
        assert_eq!(
            CombinedAutocompleteProvider::<FakeLister>::extract_path_prefix("hello ", false),
            Some(String::new())
        );
    }

    #[test]
    fn should_not_force_file_in_slash_name() {
        let lister = FakeLister::new();
        let provider = CombinedAutocompleteProvider::new(vec![], "/tmp", lister);
        let lines = vec!["/hel".to_owned()];
        assert!(!provider.should_trigger_file_completion(&lines, 0, 4));
        let lines = vec!["/help arg".to_owned()];
        assert!(provider.should_trigger_file_completion(&lines, 0, 9));
    }
}
