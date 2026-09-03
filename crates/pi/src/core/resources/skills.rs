//! Skill loading, validation, and prompt formatting.
//!
//! Port of `.references/pi-2.0/packages/coding-agent/src/core/skills.ts`.

use std::collections::HashSet;
use std::fs;
use std::path::{Path, PathBuf};

use ignore::gitignore::GitignoreBuilder;
use indexmap::IndexMap;

use crate::core::config::{
    CONFIG_DIR_NAME, PathInputOptions, canonicalize_path, get_agent_dir, resolve_path_with,
};
use crate::core::resources::diagnostics::{ResourceCollision, ResourceDiagnostic, ResourceType};
use crate::core::resources::frontmatter::{
    frontmatter_bool, frontmatter_string, parse_frontmatter, strip_frontmatter,
};
use crate::core::resources::source_info::{
    SourceInfo, SourceScope, SyntheticSourceInfoOptions, create_synthetic_source_info,
};

/// Max skill name length per Agent Skills spec.
const MAX_NAME_LENGTH: usize = 64;
/// Max skill description length per Agent Skills spec.
const MAX_DESCRIPTION_LENGTH: usize = 1024;

/// Loaded skill.
#[derive(Clone, Debug, PartialEq)]
pub struct Skill {
    /// Skill name (frontmatter or parent directory).
    pub name: String,
    /// Required description.
    pub description: String,
    /// Absolute path to the skill markdown file.
    pub file_path: String,
    /// Parent directory of the skill file.
    pub base_dir: String,
    /// Provenance.
    pub source_info: SourceInfo,
    /// When true, skill is excluded from model-facing prompt XML.
    pub disable_model_invocation: bool,
}

/// Result of loading skills.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct LoadSkillsResult {
    /// Loaded skills (first-name wins after realpath silent dedupe).
    pub skills: Vec<Skill>,
    /// Validation / collision diagnostics.
    pub diagnostics: Vec<ResourceDiagnostic>,
}

/// Options for [`load_skills_from_dir`].
#[derive(Clone, Debug)]
pub struct LoadSkillsFromDirOptions {
    /// Directory to scan.
    pub dir: PathBuf,
    /// Source label (`user` / `project` / `path` / custom).
    pub source: String,
}

/// Options for [`load_skills`].
#[derive(Clone, Debug)]
pub struct LoadSkillsOptions {
    /// Working directory.
    pub cwd: PathBuf,
    /// Agent config directory.
    pub agent_dir: PathBuf,
    /// Explicit skill paths (files or directories).
    pub skill_paths: Vec<String>,
    /// Include default `{agent}/skills` and `{cwd}/.pi/skills`.
    pub include_defaults: bool,
}

/// Load skills from a directory.
///
/// Discovery rules:
/// - if a directory contains `SKILL.md`, treat it as a skill root and do not recurse
/// - otherwise, load direct `.md` children in the root
/// - recurse into subdirectories to find `SKILL.md`
#[must_use]
pub fn load_skills_from_dir(options: &LoadSkillsFromDirOptions) -> LoadSkillsResult {
    load_skills_from_dir_internal(&options.dir, &options.source, true, None, None)
}

/// Load skills from configured locations.
#[must_use]
pub fn load_skills(options: &LoadSkillsOptions) -> LoadSkillsResult {
    let dirs = resolve_skill_dirs(options);
    let mut accumulator = SkillAccumulator::default();

    if options.include_defaults {
        load_default_skills(&dirs, &mut accumulator);
    }
    load_explicit_skills(options, &dirs, &mut accumulator);

    accumulator.finish()
}

struct ResolvedSkillDirs {
    cwd: PathBuf,
    user_skills: PathBuf,
    project_skills: PathBuf,
}

fn resolve_skill_dirs(options: &LoadSkillsOptions) -> ResolvedSkillDirs {
    let cwd = resolve_path_with(
        &path_to_string(&options.cwd),
        Path::new("."),
        PathInputOptions::new(),
    );
    let agent_input = if options.agent_dir.as_os_str().is_empty() {
        get_agent_dir()
    } else {
        options.agent_dir.clone()
    };
    let agent = resolve_path_with(
        &path_to_string(&agent_input),
        Path::new("."),
        PathInputOptions::new(),
    );
    let user_skills = agent.join("skills");
    let project_skills = cwd.join(CONFIG_DIR_NAME).join("skills");

    ResolvedSkillDirs {
        cwd,
        user_skills,
        project_skills,
    }
}

#[derive(Default)]
struct SkillAccumulator {
    skills: IndexMap<String, Skill>,
    real_paths: HashSet<String>,
    diagnostics: Vec<ResourceDiagnostic>,
    collisions: Vec<ResourceDiagnostic>,
}

impl SkillAccumulator {
    fn add(&mut self, result: LoadSkillsResult) {
        self.diagnostics.extend(result.diagnostics);
        for skill in result.skills {
            let real_path = path_to_string(&canonicalize_path(&skill.file_path));
            if self.real_paths.contains(&real_path) {
                continue;
            }
            if let Some(existing) = self.skills.get(&skill.name) {
                self.collisions.push(ResourceDiagnostic::collision(
                    format!("name \"{}\" collision", skill.name),
                    Some(skill.file_path.clone()),
                    ResourceCollision {
                        resource_type: ResourceType::Skill,
                        name: skill.name.clone(),
                        winner_path: existing.file_path.clone(),
                        loser_path: skill.file_path.clone(),
                        winner_source: None,
                        loser_source: None,
                    },
                ));
            } else {
                self.real_paths.insert(real_path);
                self.skills.insert(skill.name.clone(), skill);
            }
        }
    }

    fn finish(mut self) -> LoadSkillsResult {
        self.diagnostics.extend(self.collisions);
        LoadSkillsResult {
            skills: self.skills.into_values().collect(),
            diagnostics: self.diagnostics,
        }
    }
}

fn load_default_skills(dirs: &ResolvedSkillDirs, accumulator: &mut SkillAccumulator) {
    accumulator.add(load_skills_from_dir_internal(
        &dirs.user_skills,
        "user",
        true,
        None,
        None,
    ));
    accumulator.add(load_skills_from_dir_internal(
        &dirs.project_skills,
        "project",
        true,
        None,
        None,
    ));
}

fn load_explicit_skills(
    options: &LoadSkillsOptions,
    dirs: &ResolvedSkillDirs,
    accumulator: &mut SkillAccumulator,
) {
    for raw_path in &options.skill_paths {
        let resolved_path =
            resolve_path_with(raw_path, &dirs.cwd, PathInputOptions::new().trim(true));
        if !resolved_path.exists() {
            accumulator.diagnostics.push(ResourceDiagnostic::warning(
                "skill path does not exist",
                Some(path_to_string(&resolved_path)),
            ));
            continue;
        }

        let source = skill_path_source(options, dirs, &resolved_path);
        load_explicit_skill_path(&resolved_path, source, accumulator);
    }
}

fn skill_path_source(
    options: &LoadSkillsOptions,
    dirs: &ResolvedSkillDirs,
    resolved_path: &Path,
) -> &'static str {
    if options.include_defaults {
        "path"
    } else if is_under_path(resolved_path, &dirs.user_skills) {
        "user"
    } else if is_under_path(resolved_path, &dirs.project_skills) {
        "project"
    } else {
        "path"
    }
}

fn load_explicit_skill_path(
    resolved_path: &Path,
    source: &str,
    accumulator: &mut SkillAccumulator,
) {
    match fs::metadata(resolved_path) {
        Ok(meta) if meta.is_dir() => accumulator.add(load_skills_from_dir_internal(
            resolved_path,
            source,
            true,
            None,
            None,
        )),
        Ok(meta) if meta.is_file() && is_markdown_file(resolved_path) => {
            let result = load_skill_from_file(resolved_path, source);
            accumulator.add(LoadSkillsResult {
                skills: result.skill.into_iter().collect(),
                diagnostics: result.diagnostics,
            });
        }
        Ok(_) => accumulator.diagnostics.push(ResourceDiagnostic::warning(
            "skill path is not a markdown file",
            Some(path_to_string(resolved_path)),
        )),
        Err(error) => accumulator.diagnostics.push(ResourceDiagnostic::warning(
            error.to_string(),
            Some(path_to_string(resolved_path)),
        )),
    }
}

fn is_markdown_file(path: &Path) -> bool {
    path.extension()
        .is_some_and(|extension| extension.eq_ignore_ascii_case("md"))
}

/// Format skills for inclusion in a system prompt (XML, Agent Skills standard).
///
/// Skills with `disable_model_invocation = true` are excluded.
#[must_use]
pub fn format_skills_for_prompt(skills: &[Skill]) -> String {
    let visible: Vec<&Skill> = skills
        .iter()
        .filter(|skill| !skill.disable_model_invocation)
        .collect();
    if visible.is_empty() {
        return String::new();
    }
    let mut lines = vec![
        "\n\nThe following skills provide specialized instructions for specific tasks.".to_owned(),
        "Use the read tool to load a skill's file when the task matches its description.".to_owned(),
        "When a skill file references a relative path, resolve it against the skill directory (parent of SKILL.md / dirname of the path) and use that absolute path in tool commands.".to_owned(),
        String::new(),
        "<available_skills>".to_owned(),
    ];
    for skill in visible {
        lines.push("  <skill>".to_owned());
        lines.push(format!("    <name>{}</name>", escape_xml(&skill.name)));
        lines.push(format!(
            "    <description>{}</description>",
            escape_xml(&skill.description)
        ));
        lines.push(format!(
            "    <location>{}</location>",
            escape_xml(&skill.file_path)
        ));
        lines.push("  </skill>".to_owned());
    }
    lines.push("</available_skills>".to_owned());
    lines.join("\n")
}

/// Expand `/skill:name [args]` into a skill block, or return the original text.
///
/// # Errors
///
/// Returns an error message when the skill file cannot be read.
pub fn expand_skill_invocation(text: &str, skills: &[Skill]) -> Result<String, String> {
    if !text.starts_with("/skill:") {
        return Ok(text.to_owned());
    }
    let rest = &text["/skill:".len()..];
    let (skill_name, args) = match rest.find(' ') {
        Some(idx) => (&rest[..idx], rest[idx + 1..].trim()),
        None => (rest, ""),
    };
    let Some(skill) = skills.iter().find(|skill| skill.name == skill_name) else {
        return Ok(text.to_owned());
    };
    let content = fs::read_to_string(&skill.file_path).map_err(|error| error.to_string())?;
    let body = strip_frontmatter(&content)
        .map_err(|error| error.to_string())?
        .trim()
        .to_owned();
    let skill_block = format!(
        "<skill name=\"{}\" location=\"{}\">\nReferences are relative to {}.\n\n{}\n</skill>",
        skill.name, skill.file_path, skill.base_dir, body
    );
    if args.is_empty() {
        Ok(skill_block)
    } else {
        Ok(format!("{skill_block}\n\n{args}"))
    }
}

// ---------------------------------------------------------------------------
// Internals
// ---------------------------------------------------------------------------

struct FileSkillResult {
    skill: Option<Skill>,
    diagnostics: Vec<ResourceDiagnostic>,
}

fn load_skills_from_dir_internal(
    dir: &Path,
    source: &str,
    include_root_files: bool,
    ignore_matcher: Option<&ignore::gitignore::Gitignore>,
    root_dir: Option<&Path>,
) -> LoadSkillsResult {
    let mut skills = Vec::new();
    let mut diagnostics = Vec::new();
    if !dir.exists() {
        return LoadSkillsResult {
            skills,
            diagnostics,
        };
    }
    let root = root_dir.unwrap_or(dir);
    let ig = build_ignore(ignore_matcher, dir, root);

    let Ok(read_dir) = fs::read_dir(dir) else {
        return LoadSkillsResult {
            skills,
            diagnostics,
        };
    };
    let mut entries: Vec<_> = read_dir.filter_map(Result::ok).collect();
    entries.sort_by_key(std::fs::DirEntry::file_name);

    for entry in &entries {
        if entry.file_name() != "SKILL.md" {
            continue;
        }
        let full_path = entry.path();
        let Ok(meta) = fs::metadata(&full_path) else {
            continue;
        };
        if !meta.is_file() {
            continue;
        }
        let rel = to_posix(&relative(root, &full_path));
        if ig.matched(&rel, false).is_ignore() {
            continue;
        }
        let result = load_skill_from_file(&full_path, source);
        if let Some(skill) = result.skill {
            skills.push(skill);
        }
        diagnostics.extend(result.diagnostics);
        return LoadSkillsResult {
            skills,
            diagnostics,
        };
    }

    for entry in &entries {
        let name = entry.file_name();
        let name_str = name.to_string_lossy();
        if name_str.starts_with('.') || name_str == "node_modules" {
            continue;
        }
        let full_path = entry.path();
        let Ok(meta) = fs::metadata(&full_path) else {
            continue;
        };
        let is_directory = meta.is_dir();
        let is_file = meta.is_file();
        let rel = to_posix(&relative(root, &full_path));
        let ignore_path = if is_directory {
            format!("{rel}/")
        } else {
            rel.clone()
        };
        if ig.matched(&ignore_path, is_directory).is_ignore() {
            continue;
        }
        if is_directory {
            let sub =
                load_skills_from_dir_internal(&full_path, source, false, Some(&ig), Some(root));
            skills.extend(sub.skills);
            diagnostics.extend(sub.diagnostics);
            continue;
        }
        if !is_file || !include_root_files || !is_markdown_file(&full_path) {
            continue;
        }
        let result = load_skill_from_file(&full_path, source);
        if let Some(skill) = result.skill {
            skills.push(skill);
        }
        diagnostics.extend(result.diagnostics);
    }

    LoadSkillsResult {
        skills,
        diagnostics,
    }
}

fn load_skill_from_file(file_path: &Path, source: &str) -> FileSkillResult {
    let mut diagnostics = Vec::new();
    let raw = match fs::read_to_string(file_path) {
        Ok(content) => content,
        Err(error) => {
            diagnostics.push(ResourceDiagnostic::warning(
                error.to_string(),
                Some(path_to_string(file_path)),
            ));
            return FileSkillResult {
                skill: None,
                diagnostics,
            };
        }
    };
    let parsed = match parse_frontmatter(&raw) {
        Ok(parsed) => parsed,
        Err(error) => {
            diagnostics.push(ResourceDiagnostic::warning(
                error.to_string(),
                Some(path_to_string(file_path)),
            ));
            return FileSkillResult {
                skill: None,
                diagnostics,
            };
        }
    };
    let skill_dir = file_path
        .parent()
        .map_or_else(|| PathBuf::from("."), Path::to_path_buf);
    let parent_dir_name = skill_dir
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default();

    let description = frontmatter_string(&parsed.frontmatter, "description");
    for error in validate_description(description.as_deref()) {
        diagnostics.push(ResourceDiagnostic::warning(
            error,
            Some(path_to_string(file_path)),
        ));
    }

    let name = frontmatter_string(&parsed.frontmatter, "name").unwrap_or(parent_dir_name);
    for error in validate_name(&name) {
        diagnostics.push(ResourceDiagnostic::warning(
            error,
            Some(path_to_string(file_path)),
        ));
    }

    let Some(description) = description.filter(|value| !value.trim().is_empty()) else {
        return FileSkillResult {
            skill: None,
            diagnostics,
        };
    };

    let disable_model_invocation =
        frontmatter_bool(&parsed.frontmatter, "disable-model-invocation").unwrap_or(false);

    FileSkillResult {
        skill: Some(Skill {
            name,
            description,
            file_path: path_to_string(file_path),
            base_dir: path_to_string(&skill_dir),
            source_info: create_skill_source_info(file_path, &skill_dir, source),
            disable_model_invocation,
        }),
        diagnostics,
    }
}

fn validate_name(name: &str) -> Vec<String> {
    let mut errors = Vec::new();
    if name.len() > MAX_NAME_LENGTH {
        errors.push(format!(
            "name exceeds {MAX_NAME_LENGTH} characters ({})",
            name.len()
        ));
    }
    if !name
        .chars()
        .all(|ch| matches!(ch, 'a'..='z' | '0'..='9' | '-'))
    {
        errors.push(
            "name contains invalid characters (must be lowercase a-z, 0-9, hyphens only)"
                .to_owned(),
        );
    }
    if name.starts_with('-') || name.ends_with('-') {
        errors.push("name must not start or end with a hyphen".to_owned());
    }
    if name.contains("--") {
        errors.push("name must not contain consecutive hyphens".to_owned());
    }
    errors
}

fn validate_description(description: Option<&str>) -> Vec<String> {
    let mut errors = Vec::new();
    match description {
        None => errors.push("description is required".to_owned()),
        Some(value) if value.trim().is_empty() => {
            errors.push("description is required".to_owned());
        }
        Some(value) => {
            // TypeScript uses JS string length (UTF-16 code units).
            let len = value.encode_utf16().count();
            if len > MAX_DESCRIPTION_LENGTH {
                errors.push(format!(
                    "description exceeds {MAX_DESCRIPTION_LENGTH} characters ({len})"
                ));
            }
        }
    }
    errors
}

fn create_skill_source_info(file_path: &Path, base_dir: &Path, source: &str) -> SourceInfo {
    let path = path_to_string(file_path);
    let base = path_to_string(base_dir);
    match source {
        "user" => create_synthetic_source_info(
            path,
            SyntheticSourceInfoOptions {
                source: "local".into(),
                scope: Some(SourceScope::User),
                origin: None,
                base_dir: Some(base),
            },
        ),
        "project" => create_synthetic_source_info(
            path,
            SyntheticSourceInfoOptions {
                source: "local".into(),
                scope: Some(SourceScope::Project),
                origin: None,
                base_dir: Some(base),
            },
        ),
        "path" => create_synthetic_source_info(
            path,
            SyntheticSourceInfoOptions {
                source: "local".into(),
                scope: None,
                origin: None,
                base_dir: Some(base),
            },
        ),
        other => create_synthetic_source_info(
            path,
            SyntheticSourceInfoOptions {
                source: other.to_owned(),
                scope: None,
                origin: None,
                base_dir: Some(base),
            },
        ),
    }
}

fn escape_xml(value: &str) -> String {
    value
        .replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&apos;")
}

fn path_to_string(path: &Path) -> String {
    path.to_string_lossy().into_owned()
}

fn to_posix(path: &Path) -> String {
    path_to_string(path).replace('\\', "/")
}

fn relative(root: &Path, path: &Path) -> PathBuf {
    path.strip_prefix(root)
        .map_or_else(|_| path.to_path_buf(), Path::to_path_buf)
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

fn build_ignore(
    parent: Option<&ignore::gitignore::Gitignore>,
    dir: &Path,
    root: &Path,
) -> ignore::gitignore::Gitignore {
    let _ = parent;
    let mut builder = GitignoreBuilder::new(root);
    let mut stack = Vec::new();
    let mut current = dir.to_path_buf();
    loop {
        stack.push(current.clone());
        if current == root {
            break;
        }
        match current.parent() {
            Some(parent) if parent != current => current = parent.to_path_buf(),
            _ => break,
        }
    }
    for path in stack.into_iter().rev() {
        for filename in [".gitignore", ".ignore", ".fdignore"] {
            let ignore_path = path.join(filename);
            if !ignore_path.exists() {
                continue;
            }
            let Ok(content) = fs::read_to_string(&ignore_path) else {
                continue;
            };
            let relative_dir = relative(root, &path);
            let prefix = {
                let rel = to_posix(&relative_dir);
                if rel.is_empty() || rel == "." {
                    String::new()
                } else {
                    format!("{}/", rel.trim_end_matches('/'))
                }
            };
            for line in content.split('\n') {
                let line = line.trim_end_matches('\r');
                if let Some(pattern) = prefix_ignore_pattern(line, &prefix) {
                    let _ = builder.add_line(None, &pattern);
                }
            }
        }
    }
    builder
        .build()
        .unwrap_or_else(|_| ignore::gitignore::Gitignore::empty())
}

fn prefix_ignore_pattern(line: &str, prefix: &str) -> Option<String> {
    let trimmed = line.trim();
    if trimmed.is_empty() {
        return None;
    }
    if trimmed.starts_with('#') && !trimmed.starts_with("\\#") {
        return None;
    }
    let (negated, pattern) = if let Some(rest) = line.strip_prefix('!') {
        (true, rest)
    } else if let Some(rest) = line.strip_prefix("\\!") {
        (false, rest)
    } else {
        (false, line)
    };
    let pattern = pattern.strip_prefix('/').unwrap_or(pattern);
    let prefixed = if prefix.is_empty() {
        pattern.to_owned()
    } else {
        format!("{prefix}{pattern}")
    };
    Some(if negated {
        format!("!{prefixed}")
    } else {
        prefixed
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{SystemTime, UNIX_EPOCH};

    fn temp_root(label: &str) -> Result<PathBuf, Box<dyn std::error::Error>> {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_or(0, |duration| duration.as_nanos());
        let root = std::env::temp_dir().join(format!("pi-skills-{label}-{nanos}"));
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(&root)?;
        Ok(root)
    }

    #[test]
    fn validate_name_messages_exact() {
        assert_eq!(
            validate_name(&"a".repeat(65))[0],
            "name exceeds 64 characters (65)"
        );
        assert_eq!(
            validate_name("Bad_Name")[0],
            "name contains invalid characters (must be lowercase a-z, 0-9, hyphens only)"
        );
        assert_eq!(
            validate_name("-x")[0],
            "name must not start or end with a hyphen"
        );
        assert_eq!(
            validate_name("a--b")[0],
            "name must not contain consecutive hyphens"
        );
    }

    #[test]
    fn markdown_extension_is_ascii_case_insensitive() {
        assert!(is_markdown_file(Path::new("skill.MD")));
        assert!(is_markdown_file(Path::new("skill.mD")));
        assert!(!is_markdown_file(Path::new("skill.txt")));
    }

    #[test]
    fn description_length_uses_utf16_code_units() {
        // 1024 BMP CJK code units (1 unit each) is accepted.
        let cjk_ok = "中".repeat(1024);
        assert_eq!(cjk_ok.encode_utf16().count(), 1024);
        assert!(validate_description(Some(&cjk_ok)).is_empty());

        // 1025 CJK units is rejected with exact count in the message.
        let cjk_over = "中".repeat(1025);
        let errs = validate_description(Some(&cjk_over));
        assert_eq!(errs[0], "description exceeds 1024 characters (1025)");

        // Emoji is 2 UTF-16 units each: 512 emoji = 1024 units accepted.
        let emoji_ok = "😀".repeat(512);
        assert_eq!(emoji_ok.encode_utf16().count(), 1024);
        // Byte length is larger than 1024, so a UTF-8 check would wrongly reject.
        assert!(emoji_ok.len() > 1024);
        assert!(validate_description(Some(&emoji_ok)).is_empty());

        // 513 emoji = 1026 units rejected.
        let emoji_over = "😀".repeat(513);
        assert_eq!(emoji_over.encode_utf16().count(), 1026);
        let errs = validate_description(Some(&emoji_over));
        assert_eq!(errs[0], "description exceeds 1024 characters (1026)");
    }

    #[test]
    fn long_emoji_description_warns_but_still_loads() -> Result<(), Box<dyn std::error::Error>> {
        let root = temp_root("desc-utf16")?;
        let skill = root.join("demo");
        fs::create_dir_all(&skill)?;
        let desc = "😀".repeat(513);
        fs::write(
            skill.join("SKILL.md"),
            format!("---\nname: demo\ndescription: \"{desc}\"\n---\nbody\n"),
        )?;
        let result = load_skills_from_dir(&LoadSkillsFromDirOptions {
            dir: skill,
            source: "path".into(),
        });
        // Name-regex/length warnings still load when description is present.
        assert_eq!(result.skills.len(), 1);
        assert!(
            result
                .diagnostics
                .iter()
                .any(|d| { d.message == "description exceeds 1024 characters (1026)" })
        );
        let _ = fs::remove_dir_all(root);
        Ok(())
    }

    #[test]
    fn missing_description_drops_skill_with_warning() -> Result<(), Box<dyn std::error::Error>> {
        let root = temp_root("nodesc")?;
        let skill = root.join("demo");
        fs::create_dir_all(&skill)?;
        fs::write(skill.join("SKILL.md"), "---\nname: demo\n---\nbody\n")?;
        let result = load_skills_from_dir(&LoadSkillsFromDirOptions {
            dir: skill,
            source: "path".into(),
        });
        assert!(result.skills.is_empty());
        assert!(
            result
                .diagnostics
                .iter()
                .any(|d| d.message == "description is required")
        );
        let _ = fs::remove_dir_all(root);
        Ok(())
    }

    #[test]
    fn first_name_collision_and_realpath_silent_dedupe() -> Result<(), Box<dyn std::error::Error>> {
        let root = temp_root("collision")?;
        let a = root.join("a");
        let b = root.join("b");
        fs::create_dir_all(&a)?;
        fs::create_dir_all(&b)?;
        fs::write(
            a.join("SKILL.md"),
            "---\nname: shared\ndescription: one\n---\n",
        )?;
        fs::write(
            b.join("SKILL.md"),
            "---\nname: shared\ndescription: two\n---\n",
        )?;
        let result = load_skills(&LoadSkillsOptions {
            cwd: root.clone(),
            agent_dir: root.join("agent"),
            skill_paths: vec![
                path_to_string(&a.join("SKILL.md")),
                path_to_string(&b.join("SKILL.md")),
            ],
            include_defaults: false,
        });
        assert_eq!(result.skills.len(), 1);
        assert_eq!(result.skills[0].description, "one");
        assert!(result.diagnostics.iter().any(|d| {
            d.message == "name \"shared\" collision"
                && d.collision.as_ref().is_some_and(|c| c.name == "shared")
        }));
        let _ = fs::remove_dir_all(root);
        Ok(())
    }

    #[test]
    fn format_skills_escapes_xml_and_hides_disabled() {
        let skill = Skill {
            name: "a&b".into(),
            description: "<x>".into(),
            file_path: "/tmp/s".into(),
            base_dir: "/tmp".into(),
            source_info: create_synthetic_source_info(
                "/tmp/s",
                SyntheticSourceInfoOptions {
                    source: "local".into(),
                    scope: None,
                    origin: None,
                    base_dir: None,
                },
            ),
            disable_model_invocation: false,
        };
        let hidden = Skill {
            name: "hidden".into(),
            description: "nope".into(),
            file_path: "/tmp/h".into(),
            base_dir: "/tmp".into(),
            source_info: skill.source_info.clone(),
            disable_model_invocation: true,
        };
        let xml = format_skills_for_prompt(&[skill, hidden]);
        assert!(xml.contains("<name>a&amp;b</name>"));
        assert!(xml.contains("<description>&lt;x&gt;</description>"));
        assert!(!xml.contains("hidden"));
    }

    #[test]
    fn expand_skill_invocation_builds_block() -> Result<(), Box<dyn std::error::Error>> {
        let root = temp_root("expand")?;
        let skill_file = root.join("SKILL.md");
        fs::write(
            &skill_file,
            "---\nname: demo\ndescription: d\n---\n\nDo the thing.\n",
        )?;
        let skill = Skill {
            name: "demo".into(),
            description: "d".into(),
            file_path: path_to_string(&skill_file),
            base_dir: path_to_string(&root),
            source_info: create_synthetic_source_info(
                path_to_string(&skill_file),
                SyntheticSourceInfoOptions {
                    source: "local".into(),
                    scope: None,
                    origin: None,
                    base_dir: Some(path_to_string(&root)),
                },
            ),
            disable_model_invocation: false,
        };
        let expanded = expand_skill_invocation("/skill:demo extra args", &[skill])
            .map_err(std::io::Error::other)?;
        assert!(expanded.contains("<skill name=\"demo\""));
        assert!(expanded.contains("Do the thing."));
        assert!(expanded.ends_with("extra args"));
        let _ = fs::remove_dir_all(root);
        Ok(())
    }

    #[test]
    fn is_under_path_rejects_prefix_without_separator() {
        let root = Path::new("/tmp/agent/skills");
        assert!(is_under_path(Path::new("/tmp/agent/skills"), root));
        assert!(is_under_path(
            Path::new("/tmp/agent/skills/demo/SKILL.md"),
            root
        ));
        assert!(!is_under_path(
            Path::new("/tmp/agent/skills-extra/x.md"),
            root
        ));
    }

    #[test]
    fn first_wins_preserves_insertion_order() -> Result<(), Box<dyn std::error::Error>> {
        let root = temp_root("order")?;
        let a = root.join("a.md");
        let b = root.join("b.md");
        let c = root.join("c.md");
        fs::write(&a, "---\nname: alpha\ndescription: a\n---\n")?;
        fs::write(&b, "---\nname: beta\ndescription: b\n---\n")?;
        fs::write(&c, "---\nname: gamma\ndescription: c\n---\n")?;
        let result = load_skills(&LoadSkillsOptions {
            cwd: root.clone(),
            agent_dir: root.join("agent"),
            skill_paths: vec![path_to_string(&a), path_to_string(&b), path_to_string(&c)],
            include_defaults: false,
        });
        let names: Vec<_> = result.skills.iter().map(|s| s.name.as_str()).collect();
        assert_eq!(names, vec!["alpha", "beta", "gamma"]);
        let _ = fs::remove_dir_all(root);
        Ok(())
    }
}
