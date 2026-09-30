//! Editor autocomplete wiring: slash-command catalog assembly plus the
//! production [`FileLister`] behind `@` path completion.
//!
//! The provider itself lives in `pi_tui` (product-agnostic); this module
//! supplies the product-owned pieces — the command catalog and the
//! filesystem backend — mirroring the TypeScript `interactive-mode`
//! `setAutocompleteProvider` setup.

use std::path::{Path, PathBuf};
use std::sync::{Arc, OnceLock};

use pi_tui::components::editor::Editor;
use pi_tui::editor_support::{CombinedAutocompleteProvider, FileEntry, FileLister, SlashCommand};

use super::runtime::SessionHost;

/// Cap on entries the std-walk fallback collects before ranking. `fd`
/// callers get `max_results` server-side; the walk needs a hard bound so
/// completion stays cheap in very large trees.
const MAX_WALK_ENTRIES: usize = 20_000;

/// Filesystem backend for `@` completion. Prefers `fd` when it resolves on
/// `PATH` (fast, honors `.gitignore`); otherwise walks with `std::fs`,
/// skipping `.git` subtrees, until `MAX_WALK_ENTRIES` entries are seen.
struct FsFileLister;

impl FsFileLister {
    /// Resolved `fd` executable, memoized. `None` means not on `PATH`.
    fn fd_path() -> Option<&'static PathBuf> {
        static FD: OnceLock<Option<PathBuf>> = OnceLock::new();
        FD.get_or_init(|| {
            let path = std::env::var_os("PATH")?;
            std::env::split_paths(&path)
                .map(|dir| dir.join("fd"))
                .find(|candidate| candidate.is_file())
        })
        .as_ref()
    }

    /// `fd`-backed walk mirroring the TypeScript argument set: files and
    /// directories, follow links, include hidden, exclude `.git`,
    /// `--full-path` when the query contains `/`. `fd` marks directories by
    /// a trailing `/`.
    fn walk_fd(base_dir: &Path, query: &str, max_results: usize) -> Option<Vec<FileEntry>> {
        let fd = Self::fd_path()?;
        let mut args = vec![
            "--base-directory".to_owned(),
            base_dir.to_string_lossy().into_owned(),
            "--max-results".to_owned(),
            max_results.to_string(),
            "--type".to_owned(),
            "f".to_owned(),
            "--type".to_owned(),
            "d".to_owned(),
            "--follow".to_owned(),
            "--hidden".to_owned(),
            "--exclude".to_owned(),
            ".git".to_owned(),
            "--exclude".to_owned(),
            ".git/*".to_owned(),
            "--exclude".to_owned(),
            ".git/**".to_owned(),
        ];
        if query.contains('/') {
            args.push("--full-path".to_owned());
        }
        // `--` keeps a leading-hyphen query (e.g. `@-foo`) from being
        // parsed as an option.
        args.push("--".to_owned());
        if !query.is_empty() {
            args.push(query.to_owned());
        }
        let output = std::process::Command::new(fd)
            .args(args)
            .stdin(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .output()
            .ok()?;
        // A failed `fd` (bad pattern, unreadable root) yields empty stdout;
        // treat it as unavailable so the std-walk fallback still runs.
        if !output.status.success() {
            return None;
        }
        let entries = String::from_utf8_lossy(&output.stdout)
            .lines()
            .map(|line| FileEntry {
                path: line.trim_end_matches('/').to_owned(),
                is_directory: line.ends_with('/'),
            })
            .collect();
        Some(entries)
    }

    /// Depth-first `std::fs` walk under `base_dir`, skipping `.git`. Entries
    /// are `/`-separated paths relative to `base_dir`; collection stops at
    /// `MAX_WALK_ENTRIES` so the provider's ranking stays bounded.
    fn walk_fs(base_dir: &Path) -> Vec<FileEntry> {
        fn visit(dir: &Path, prefix: &mut String, entries: &mut Vec<FileEntry>) {
            let Ok(read_dir) = std::fs::read_dir(dir) else {
                return;
            };
            for entry in read_dir.flatten() {
                if entries.len() >= MAX_WALK_ENTRIES {
                    return;
                }
                let name = entry.file_name().to_string_lossy().into_owned();
                let is_symlink = entry.file_type().is_ok_and(|t| t.is_symlink());
                let is_directory = is_dir_entry(&entry);
                let saved = prefix.len();
                if !prefix.is_empty() {
                    prefix.push('/');
                }
                prefix.push_str(&name);
                if is_directory && name != ".git" {
                    entries.push(FileEntry {
                        path: prefix.clone(),
                        is_directory: true,
                    });
                    // Symlinked dirs complete as directories but are not
                    // recursed: a link cycle would spin the walk until the
                    // entry cap without yielding new paths.
                    if !is_symlink {
                        visit(&entry.path(), prefix, entries);
                    }
                } else if !is_directory {
                    entries.push(FileEntry {
                        path: prefix.clone(),
                        is_directory: false,
                    });
                }
                prefix.truncate(saved);
            }
        }
        let mut entries = Vec::new();
        visit(base_dir, &mut String::new(), &mut entries);
        entries
    }
}

/// Directory check that follows symlinks: `DirEntry::file_type` reports the
/// link itself, so a symlink-to-directory needs the target stat.
fn is_dir_entry(entry: &std::fs::DirEntry) -> bool {
    match entry.file_type() {
        Ok(t) => t.is_dir() || (t.is_symlink() && entry.path().is_dir()),
        Err(_) => false,
    }
}

impl FileLister for FsFileLister {
    fn list_dir(&self, dir: &Path) -> Vec<FileEntry> {
        let Ok(read_dir) = std::fs::read_dir(dir) else {
            return Vec::new();
        };
        read_dir
            .flatten()
            .map(|entry| FileEntry {
                path: entry.file_name().to_string_lossy().into_owned(),
                is_directory: is_dir_entry(&entry),
            })
            .collect()
    }

    fn fuzzy_walk(
        &self,
        base_dir: &Path,
        query: &str,
        max_results: usize,
        cancelled: bool,
    ) -> Vec<FileEntry> {
        if cancelled {
            return Vec::new();
        }
        if let Some(entries) = Self::walk_fd(base_dir, query, max_results) {
            return entries;
        }
        let query_lower = query.to_ascii_lowercase();
        Self::walk_fs(base_dir)
            .into_iter()
            .filter(|entry| {
                query.is_empty() || entry.path.to_ascii_lowercase().contains(&query_lower)
            })
            .take(max_results)
            .collect()
    }
}

/// Rebuild the editor's autocomplete provider from the live catalog:
/// builtin slash commands first, then the session's extension / prompt /
/// skill commands (first name wins on collision).
pub(super) fn refresh_autocomplete<S: SessionHost>(editor: &mut Editor, session: &Arc<S>) {
    let mut seen = std::collections::HashSet::new();
    let mut commands = Vec::new();
    for builtin in crate::core::resources::slash::builtin_slash_commands() {
        if seen.insert(builtin.name.clone()) {
            commands.push(SlashCommand {
                name: builtin.name,
                description: Some(builtin.description),
                argument_hint: builtin.argument_hint,
                argument_completions: None,
            });
        }
    }
    for info in session.slash_commands() {
        if seen.insert(info.name.clone()) {
            commands.push(SlashCommand {
                name: info.name,
                description: info.description,
                argument_hint: None,
                argument_completions: None,
            });
        }
    }
    let base_path = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
    editor.set_autocomplete_provider(Some(Arc::new(CombinedAutocompleteProvider::new(
        commands,
        base_path,
        FsFileLister,
    ))));
}
