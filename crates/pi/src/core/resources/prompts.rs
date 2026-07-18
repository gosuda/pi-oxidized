//! Prompt template loading and argument substitution.
//!
//! Port of `.references/pi/packages/coding-agent/src/core/prompt-templates.ts`.

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::LazyLock;

use regex::Regex;

use crate::core::config::{CONFIG_DIR_NAME, PathInputOptions, resolve_path_with};
use crate::core::resources::frontmatter::{frontmatter_string, parse_frontmatter};
use crate::core::resources::source_info::{
    SourceInfo, SourceScope, SyntheticSourceInfoOptions, create_synthetic_source_info,
};

/// Prompt template loaded from a markdown file.
#[derive(Clone, Debug, PartialEq)]
pub struct PromptTemplate {
    /// Template name (filename without `.md`).
    pub name: String,
    /// Description from frontmatter or first body line.
    pub description: String,
    /// Optional argument hint from frontmatter `argument-hint`.
    pub argument_hint: Option<String>,
    /// Body content after frontmatter.
    pub content: String,
    /// Provenance.
    pub source_info: SourceInfo,
    /// Absolute path to the template file.
    pub file_path: String,
}

/// Options for [`load_prompt_templates`].
#[derive(Clone, Debug)]
pub struct LoadPromptTemplatesOptions {
    /// Working directory.
    pub cwd: PathBuf,
    /// Agent config directory.
    pub agent_dir: PathBuf,
    /// Explicit prompt paths (files or directories).
    pub prompt_paths: Vec<String>,
    /// Include default global/project prompt directories.
    pub include_defaults: bool,
}

static SUBSTITUTE_ARGS_RE: LazyLock<Option<Regex>> = LazyLock::new(|| {
    Regex::new(r"\$\{(\d+):-([^}]*)\}|\$\{@:(\d+)(?::(\d+))?\}|\$(ARGUMENTS|@|\d+)").ok()
});

static EXPAND_PROMPT_RE: LazyLock<Option<Regex>> =
    LazyLock::new(|| Regex::new(r"^/([^\s]+)(?:\s+([\s\S]*))?$").ok());

/// Parse command arguments respecting quoted strings (bash-style).
#[must_use]
pub fn parse_command_args(args_string: &str) -> Vec<String> {
    let mut args = Vec::new();
    let mut current = String::new();
    let mut in_quote: Option<char> = None;

    for char in args_string.chars() {
        if let Some(quote) = in_quote {
            if char == quote {
                in_quote = None;
            } else {
                current.push(char);
            }
        } else if char == '"' || char == '\'' {
            in_quote = Some(char);
        } else if char.is_whitespace() {
            if !current.is_empty() {
                args.push(std::mem::take(&mut current));
            }
        } else {
            current.push(char);
        }
    }
    if !current.is_empty() {
        args.push(current);
    }
    args
}

/// Substitute argument placeholders in template content.
///
/// Supports:
/// - `$1`, `$2`, … positional args
/// - `$@` and `$ARGUMENTS` for all args
/// - `${N:-default}` for positional arg N with default when missing/empty
/// - `${@:N}` for args from Nth onwards
/// - `${@:N:L}` for L args starting from Nth
///
/// Replacement is non-recursive on argument/default values.
#[must_use]
pub fn substitute_args(content: &str, args: &[String]) -> String {
    let all_args = args.join(" ");
    let Some(re) = SUBSTITUTE_ARGS_RE.as_ref() else {
        return content.to_owned();
    };
    re.replace_all(content, |caps: &regex::Captures<'_>| {
        if let Some(default_num) = caps.get(1) {
            let index: usize = default_num.as_str().parse::<usize>().unwrap_or(1) - 1;
            let default_value = caps.get(2).map_or("", |m| m.as_str());
            let value = args.get(index).map_or("", String::as_str);
            if value.is_empty() {
                default_value.to_owned()
            } else {
                value.to_owned()
            }
        } else if let Some(slice_start) = caps.get(3) {
            let start = slice_start.as_str().parse::<isize>().unwrap_or(1) - 1;
            let start = usize::try_from(start.max(0)).unwrap_or_default();
            if let Some(slice_length) = caps.get(4) {
                let length: usize = slice_length.as_str().parse().unwrap_or(0);
                args.iter()
                    .skip(start)
                    .take(length)
                    .cloned()
                    .collect::<Vec<_>>()
                    .join(" ")
            } else {
                args.iter()
                    .skip(start)
                    .cloned()
                    .collect::<Vec<_>>()
                    .join(" ")
            }
        } else {
            let simple = caps.get(5).map_or("", |m| m.as_str());
            if simple == "ARGUMENTS" || simple == "@" {
                all_args.clone()
            } else {
                let index: usize = simple.parse::<usize>().unwrap_or(1) - 1;
                args.get(index).cloned().unwrap_or_default()
            }
        }
    })
    .into_owned()
}

/// Load all prompt templates from defaults and/or explicit paths.
///
/// Directory scans are non-recursive (`.md` only).
#[must_use]
pub fn load_prompt_templates(options: &LoadPromptTemplatesOptions) -> Vec<PromptTemplate> {
    let resolved_cwd = resolve_path_with(
        &path_to_string(&options.cwd),
        Path::new("."),
        PathInputOptions::new(),
    );
    let resolved_agent_dir = resolve_path_with(
        &path_to_string(&options.agent_dir),
        Path::new("."),
        PathInputOptions::new(),
    );
    let mut templates = Vec::new();

    let global_prompts_dir = resolved_agent_dir.join("prompts");
    let project_prompts_dir = resolved_cwd.join(CONFIG_DIR_NAME).join("prompts");

    let get_source_info = |resolved_path: &Path| -> SourceInfo {
        if is_under_path(resolved_path, &global_prompts_dir) {
            return create_synthetic_source_info(
                path_to_string(resolved_path),
                SyntheticSourceInfoOptions {
                    source: "local".into(),
                    scope: Some(SourceScope::User),
                    origin: None,
                    base_dir: Some(path_to_string(&global_prompts_dir)),
                },
            );
        }
        if is_under_path(resolved_path, &project_prompts_dir) {
            return create_synthetic_source_info(
                path_to_string(resolved_path),
                SyntheticSourceInfoOptions {
                    source: "local".into(),
                    scope: Some(SourceScope::Project),
                    origin: None,
                    base_dir: Some(path_to_string(&project_prompts_dir)),
                },
            );
        }
        let base_dir = if resolved_path.is_dir() {
            path_to_string(resolved_path)
        } else {
            resolved_path
                .parent()
                .map_or_else(|| path_to_string(resolved_path), path_to_string)
        };
        create_synthetic_source_info(
            path_to_string(resolved_path),
            SyntheticSourceInfoOptions {
                source: "local".into(),
                scope: None,
                origin: None,
                base_dir: Some(base_dir),
            },
        )
    };

    if options.include_defaults {
        templates.extend(load_templates_from_dir(
            &global_prompts_dir,
            &get_source_info,
        ));
        templates.extend(load_templates_from_dir(
            &project_prompts_dir,
            &get_source_info,
        ));
    }

    for raw_path in &options.prompt_paths {
        let resolved_path =
            resolve_path_with(raw_path, &resolved_cwd, PathInputOptions::new().trim(true));
        if !resolved_path.exists() {
            continue;
        }
        match fs::metadata(&resolved_path) {
            Ok(meta) if meta.is_dir() => {
                templates.extend(load_templates_from_dir(&resolved_path, &get_source_info));
            }
            Ok(meta)
                if meta.is_file()
                    && resolved_path
                        .extension()
                        .is_some_and(|ext| ext.eq_ignore_ascii_case("md")) =>
            {
                if let Some(template) =
                    load_template_from_file(&resolved_path, get_source_info(&resolved_path))
                {
                    templates.push(template);
                }
            }
            _ => {}
        }
    }

    templates
}

/// Expand a `/template [args]` command, or return the original text.
#[must_use]
pub fn expand_prompt_template(text: &str, templates: &[PromptTemplate]) -> String {
    if !text.starts_with('/') {
        return text.to_owned();
    }
    let Some(re) = EXPAND_PROMPT_RE.as_ref() else {
        return text.to_owned();
    };
    let Some(caps) = re.captures(text) else {
        return text.to_owned();
    };
    let template_name = caps.get(1).map_or("", |m| m.as_str());
    let args_string = caps.get(2).map_or("", |m| m.as_str());
    let Some(template) = templates.iter().find(|t| t.name == template_name) else {
        return text.to_owned();
    };
    let args = parse_command_args(args_string);
    substitute_args(&template.content, &args)
}

fn load_templates_from_dir(
    dir: &Path,
    get_source_info: &dyn Fn(&Path) -> SourceInfo,
) -> Vec<PromptTemplate> {
    let mut templates = Vec::new();
    if !dir.exists() {
        return templates;
    }
    let Ok(read_dir) = fs::read_dir(dir) else {
        return templates;
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
                .is_some_and(|ext| ext.eq_ignore_ascii_case("md"))
            && let Some(template) = load_template_from_file(&full_path, get_source_info(&full_path))
        {
            templates.push(template);
        }
    }
    templates
}

fn load_template_from_file(file_path: &Path, source_info: SourceInfo) -> Option<PromptTemplate> {
    let raw = fs::read_to_string(file_path).ok()?;
    let parsed = parse_frontmatter(&raw).ok()?;
    let name = file_path
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default()
        .trim_end_matches(".md")
        .to_owned();

    let mut description =
        frontmatter_string(&parsed.frontmatter, "description").unwrap_or_default();
    if description.is_empty()
        && let Some(first_line) = parsed.body.split('\n').find(|line| !line.trim().is_empty())
    {
        // TypeScript uses JS string length / slice (UTF-16 code units).
        // Scalar-safe port: count UTF-16 units; if a boundary would bisect a
        // surrogate pair (non-BMP scalar), include the whole scalar so we never
        // emit a lone surrogate (documented normalization vs pure JS).
        description = truncate_utf16_units(first_line, 60);
        if utf16_len(first_line) > 60 {
            description.push_str("...");
        }
    }

    let argument_hint = frontmatter_string(&parsed.frontmatter, "argument-hint");

    Some(PromptTemplate {
        name,
        description,
        argument_hint,
        content: parsed.body,
        source_info,
        file_path: path_to_string(file_path),
    })
}

fn path_to_string(path: &Path) -> String {
    path.to_string_lossy().into_owned()
}

/// UTF-16 code unit length (JS `String.length`).
fn utf16_len(s: &str) -> usize {
    s.encode_utf16().count()
}

/// Truncate to at most `max_units` UTF-16 code units without splitting a scalar.
///
/// If the 60-unit boundary falls inside a non-BMP scalar (surrogate pair), the
/// whole scalar is kept so the result never contains a lone surrogate. That is
/// the only intentional divergence from raw JS `slice` when a boundary bisects
/// a surrogate pair.
fn truncate_utf16_units(s: &str, max_units: usize) -> String {
    if max_units == 0 {
        return String::new();
    }
    let mut units = 0usize;
    let mut end = 0usize;
    for (idx, ch) in s.char_indices() {
        let ch_units = ch.len_utf16();
        if units + ch_units > max_units {
            // Scalar-safe: if this is a multi-unit scalar that straddles the
            // boundary (units < max_units < units+ch_units), keep the whole
            // scalar so we never emit a lone surrogate. Pure ASCII that would
            // start past max_units is dropped (strict JS slice).
            if ch_units > 1 && units < max_units {
                end = idx + ch.len_utf8();
            }
            break;
        }
        units += ch_units;
        end = idx + ch.len_utf8();
        if units == max_units {
            break;
        }
    }
    s[..end].to_owned()
}

fn is_under_path(target: &Path, root: &Path) -> bool {
    let normalized_root = resolve_path_with(
        &path_to_string(root),
        Path::new("."),
        PathInputOptions::new(),
    );
    let normalized_target = resolve_path_with(
        &path_to_string(target),
        Path::new("."),
        PathInputOptions::new(),
    );
    if normalized_target == normalized_root {
        return true;
    }
    // Match TS: prefix is root + separator — never `/skills-extra` for `/skills`.
    let root_str = path_to_string(&normalized_root);
    let target_str = path_to_string(&normalized_target);
    let sep = std::path::MAIN_SEPARATOR;
    let prefix = if root_str.ends_with(sep) {
        root_str
    } else {
        format!("{root_str}{sep}")
    };
    target_str.starts_with(&prefix)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{SystemTime, UNIX_EPOCH};

    fn temp_root(label: &str) -> std::io::Result<PathBuf> {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_or(0, |duration| duration.as_nanos());
        let root = std::env::temp_dir().join(format!("pi-prompts-{label}-{nanos}"));
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(&root)?;
        Ok(root)
    }

    #[test]
    fn parse_command_args_respects_quotes() {
        assert_eq!(
            parse_command_args(r#"one "two three" 'four five' six"#),
            vec!["one", "two three", "four five", "six"]
        );
    }

    #[test]
    fn substitute_args_positional_defaults_and_slices() {
        let args = vec!["a".into(), "b".into(), "c".into()];
        assert_eq!(substitute_args("x $1 y $2", &args), "x a y b");
        assert_eq!(substitute_args("all=$@", &args), "all=a b c");
        assert_eq!(substitute_args("all=$ARGUMENTS", &args), "all=a b c");
        assert_eq!(substitute_args("${2:-fallback}", &[]), "fallback");
        assert_eq!(substitute_args("${2:-fallback}", &args), "b");
        assert_eq!(substitute_args("${@:2}", &args), "b c");
        assert_eq!(substitute_args("${@:2:1}", &args), "b");
        // non-recursive on values
        assert_eq!(substitute_args("$1", &["$2".into(), "b".into()]), "$2");
    }

    #[test]
    fn load_prompt_templates_nonrecursive() -> std::io::Result<()> {
        let root = temp_root("nonrec")?;
        let agent = root.join("agent");
        let prompts = agent.join("prompts");
        fs::create_dir_all(prompts.join("nested"))?;
        fs::write(
            prompts.join("hello.md"),
            "---\ndescription: hi\n---\nHello $1\n",
        )?;
        fs::write(prompts.join("nested").join("skip.md"), "nope\n")?;
        let templates = load_prompt_templates(&LoadPromptTemplatesOptions {
            cwd: root.join("cwd"),
            agent_dir: agent,
            prompt_paths: vec![],
            include_defaults: true,
        });
        assert_eq!(templates.len(), 1);
        assert_eq!(templates[0].name, "hello");
        assert_eq!(templates[0].description, "hi");
        let expanded = expand_prompt_template("/hello world", &templates);
        assert_eq!(expanded, "Hello world");
        let _ = fs::remove_dir_all(root);
        Ok(())
    }

    #[test]
    fn auto_description_uses_utf16_units_with_emoji() -> std::io::Result<()> {
        // 59 ASCII + one emoji (2 UTF-16 units) => length 61 in JS; slice(0,60)
        // would bisect the surrogate pair. Scalar-safe port keeps whole emoji
        // (61 units) then still appends "..." because original > 60 units.
        let ascii = "a".repeat(59);
        let emoji = "😀"; // U+1F600, 2 UTF-16 units
        let line = format!("{ascii}{emoji}x");
        assert_eq!(utf16_len(&line), 62);
        let truncated = truncate_utf16_units(&line, 60);
        // 59 ASCII + full emoji = 61 units (no lone surrogate)
        assert_eq!(utf16_len(&truncated), 61);
        assert!(truncated.ends_with(emoji));
        assert!(!truncated.ends_with('x'));

        // Pure ASCII: exact 60 + ellipsis
        let long = "b".repeat(70);
        let t = truncate_utf16_units(&long, 60);
        assert_eq!(t.len(), 60);
        assert_eq!(utf16_len(&t), 60);

        let root = temp_root("desc")?;
        let agent = root.join("agent");
        let prompts = agent.join("prompts");
        fs::create_dir_all(&prompts)?;
        let body_line = format!("{ascii}{emoji}TAIL");
        fs::write(prompts.join("e.md"), format!("{body_line}\n"))?;
        let templates = load_prompt_templates(&LoadPromptTemplatesOptions {
            cwd: root.join("cwd"),
            agent_dir: agent,
            prompt_paths: vec![],
            include_defaults: true,
        });
        assert_eq!(templates.len(), 1);
        assert!(templates[0].description.ends_with("..."));
        assert!(templates[0].description.contains(emoji));
        let _ = fs::remove_dir_all(root);
        Ok(())
    }

    #[test]
    fn is_under_path_requires_separator_boundary() {
        let root = Path::new("/tmp/agent/skills");
        let child = Path::new("/tmp/agent/skills/demo/SKILL.md");
        let sibling = Path::new("/tmp/agent/skills-extra/demo/SKILL.md");
        assert!(is_under_path(child, root));
        assert!(is_under_path(root, root));
        assert!(!is_under_path(sibling, root));
    }
}
