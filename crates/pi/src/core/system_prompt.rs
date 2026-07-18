//! System prompt construction for the coding agent.
//!
//! Port of `.references/pi/packages/coding-agent/src/core/system-prompt.ts`.
//! Pure assembly of identity, tools, guidelines, docs paths, append text,
//! project context XML, skills XML, and cwd — no date/time injection.

use std::collections::{HashMap, HashSet};
use std::path::Path;

use crate::core::config::{get_docs_path, get_examples_path, get_readme_path};
use crate::core::resources::{AgentsFile, Skill, format_skills_for_prompt};

/// Default tool names when [`BuildSystemPromptOptions::selected_tools`] is omitted.
const DEFAULT_SELECTED_TOOLS: [&str; 4] = ["read", "bash", "edit", "write"];

/// Options for [`build_system_prompt`].
///
/// Mirrors TypeScript `BuildSystemPromptOptions`. Field names are `snake_case`;
/// wire callers join multi-value append sources with `"\n\n"` before setting
/// [`Self::append`].
#[derive(Clone, Debug, Default)]
pub struct BuildSystemPromptOptions {
    /// Custom system prompt body (replaces the default identity/tools/guidelines/docs block).
    ///
    /// Empty and `None` both take the default path (matches JS truthiness on `customPrompt`).
    pub custom_prompt: Option<String>,
    /// Tools considered active for snippets, guidelines, and the skills gate.
    ///
    /// When `None`, defaults to `read`, `bash`, `edit`, `write`. An empty `Vec`
    /// means no tools.
    pub selected_tools: Option<Vec<String>>,
    /// One-line tool snippets keyed by tool name. A tool appears under Available
    /// tools only when its snippet is present and non-empty.
    pub tool_snippets: Option<HashMap<String, String>>,
    /// Extra guideline bullets (trimmed, de-duplicated, first-add wins).
    pub prompt_guidelines: Option<Vec<String>>,
    /// Text appended after the base/custom body (before project context).
    ///
    /// Empty and `None` omit the append section.
    pub append: Option<String>,
    /// Working directory (required). Backslashes are normalized to `/` in the
    /// final cwd line only.
    pub cwd: String,
    /// Pre-loaded project context files (`AGENTS.md` / `CLAUDE.md`).
    pub context_files: Option<Vec<AgentsFile>>,
    /// Pre-loaded skills (formatted via [`format_skills_for_prompt`]).
    pub skills: Option<Vec<Skill>>,
}

/// Build the system prompt with tools, guidelines, project context, skills, and cwd.
///
/// # Branch order
///
/// **Custom** (`custom_prompt` non-empty):
/// 1. raw custom body
/// 2. append section (if any)
/// 3. project context XML (if any)
/// 4. skills XML when the read tool is available (or `selected_tools` is `None`)
/// 5. cwd line
///
/// **Default**:
/// 1. identity + Available tools + custom-tools sentence + Guidelines + Pi docs
/// 2. append section (if any)
/// 3. project context XML (if any)
/// 4. skills XML when `read` is among selected tools
/// 5. cwd line
///
/// No date or time is injected.
#[must_use]
pub fn build_system_prompt(options: &BuildSystemPromptOptions) -> String {
    let prompt_cwd = options.cwd.replace('\\', "/");
    let append_section = match options.append.as_deref() {
        Some(text) if !text.is_empty() => format!("\n\n{text}"),
        _ => String::new(),
    };
    let context_files = options.context_files.as_deref().unwrap_or(&[]);
    let skills = options.skills.as_deref().unwrap_or(&[]);

    if let Some(custom) = options.custom_prompt.as_deref()
        && !custom.is_empty()
    {
        return build_custom_prompt(
            custom,
            &append_section,
            context_files,
            skills,
            options.selected_tools.as_deref(),
            &prompt_cwd,
        );
    }

    build_default_prompt(options, &append_section, context_files, skills, &prompt_cwd)
}

fn build_custom_prompt(
    custom: &str,
    append_section: &str,
    context_files: &[AgentsFile],
    skills: &[Skill],
    selected_tools: Option<&[String]>,
    prompt_cwd: &str,
) -> String {
    let mut prompt = custom.to_owned();
    if !append_section.is_empty() {
        prompt.push_str(append_section);
    }
    append_project_context(&mut prompt, context_files);

    // Skills when read is available: omitted selected_tools ⇒ treat as has read.
    let custom_has_read =
        selected_tools.is_none_or(|tools| tools.iter().any(|tool| tool == "read"));
    if custom_has_read && !skills.is_empty() {
        prompt.push_str(&format_skills_for_prompt(skills));
    }

    prompt.push_str("\nCurrent working directory: ");
    prompt.push_str(prompt_cwd);
    prompt
}

fn build_default_prompt(
    options: &BuildSystemPromptOptions,
    append_section: &str,
    context_files: &[AgentsFile],
    skills: &[Skill],
    prompt_cwd: &str,
) -> String {
    let readme_path = path_display(&get_readme_path());
    let docs_path = path_display(&get_docs_path());
    let examples_path = path_display(&get_examples_path());

    let default_tools: Vec<String> = DEFAULT_SELECTED_TOOLS
        .iter()
        .map(|name| (*name).to_owned())
        .collect();
    let tools: &[String] = options
        .selected_tools
        .as_deref()
        .unwrap_or(default_tools.as_slice());

    let tools_list = build_tools_list(tools, options.tool_snippets.as_ref());
    let guidelines = build_guidelines(tools, options.prompt_guidelines.as_deref());

    let mut prompt = format!(
        "You are an expert coding assistant operating inside pi, a coding agent harness. You help users by reading files, executing commands, editing code, and writing new files.\n\
         \n\
         Available tools:\n\
         {tools_list}\n\
         \n\
         In addition to the tools above, you may have access to other custom tools depending on the project.\n\
         \n\
         Guidelines:\n\
         {guidelines}\n\
         \n\
         Pi documentation (read only when the user asks about pi itself, its SDK, extensions, themes, skills, or TUI):\n\
         - Main documentation: {readme_path}\n\
         - Additional docs: {docs_path}\n\
         - Examples: {examples_path} (extensions, custom tools, SDK)\n\
         - When reading pi docs or examples, resolve docs/... under Additional docs and examples/... under Examples, not the current working directory\n\
         - When asked about: extensions (docs/extensions.md, examples/extensions/), themes (docs/themes.md), skills (docs/skills.md), prompt templates (docs/prompt-templates.md), TUI components (docs/tui.md), keybindings (docs/keybindings.md), SDK integrations (docs/sdk.md), custom providers (docs/custom-provider.md), adding models (docs/models.md), pi packages (docs/packages.md)\n\
         - When working on pi topics, read the docs and examples, and follow .md cross-references before implementing\n\
         - Always read pi .md files completely and follow links to related docs (e.g., tui.md for TUI API details)"
    );

    if !append_section.is_empty() {
        prompt.push_str(append_section);
    }
    append_project_context(&mut prompt, context_files);

    let has_read = tools.iter().any(|t| t == "read");
    if has_read && !skills.is_empty() {
        prompt.push_str(&format_skills_for_prompt(skills));
    }

    prompt.push_str("\nCurrent working directory: ");
    prompt.push_str(prompt_cwd);
    prompt
}

fn build_tools_list(tools: &[String], tool_snippets: Option<&HashMap<String, String>>) -> String {
    let Some(snippets) = tool_snippets else {
        return "(none)".to_owned();
    };

    let mut lines: Vec<String> = Vec::new();
    for name in tools {
        if let Some(snippet) = snippets.get(name.as_str())
            && !snippet.is_empty()
        {
            lines.push(format!("- {name}: {snippet}"));
        }
    }

    if lines.is_empty() {
        "(none)".to_owned()
    } else {
        lines.join("\n")
    }
}

fn build_guidelines(tools: &[String], prompt_guidelines: Option<&[String]>) -> String {
    let mut guidelines_list: Vec<String> = Vec::new();
    let mut guidelines_set: HashSet<String> = HashSet::new();
    let mut add_guideline = |guideline: &str| {
        if guidelines_set.insert(guideline.to_owned()) {
            guidelines_list.push(guideline.to_owned());
        }
    };

    let has_bash = tools.iter().any(|t| t == "bash");
    let has_grep = tools.iter().any(|t| t == "grep");
    let has_find = tools.iter().any(|t| t == "find");
    let has_ls = tools.iter().any(|t| t == "ls");

    if has_bash && !has_grep && !has_find && !has_ls {
        add_guideline("Use bash for file operations like ls, rg, find");
    }

    if let Some(extra) = prompt_guidelines {
        for guideline in extra {
            let normalized = guideline.trim();
            if !normalized.is_empty() {
                add_guideline(normalized);
            }
        }
    }

    add_guideline("Be concise in your responses");
    add_guideline("Show file paths clearly when working with files");

    guidelines_list
        .iter()
        .map(|g| format!("- {g}"))
        .collect::<Vec<_>>()
        .join("\n")
}

/// Project context XML. Path attribute and content are inserted raw (TypeScript parity).
fn append_project_context(prompt: &mut String, context_files: &[AgentsFile]) {
    if context_files.is_empty() {
        return;
    }
    prompt.push_str("\n\n<project_context>\n\n");
    prompt.push_str("Project-specific instructions and guidelines:\n\n");
    for file in context_files {
        prompt.push_str("<project_instructions path=\"");
        prompt.push_str(&file.path);
        prompt.push_str("\">\n");
        prompt.push_str(&file.content);
        prompt.push_str("\n</project_instructions>\n\n");
    }
    prompt.push_str("</project_context>\n");
}

fn path_display(path: &Path) -> String {
    path.to_string_lossy().into_owned()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::resources::source_info::{
        SyntheticSourceInfoOptions, create_synthetic_source_info,
    };

    fn snippets(pairs: &[(&str, &str)]) -> HashMap<String, String> {
        pairs
            .iter()
            .map(|(k, v)| ((*k).to_owned(), (*v).to_owned()))
            .collect()
    }

    fn default_tool_snippets() -> HashMap<String, String> {
        snippets(&[
            ("read", "Read file contents"),
            ("bash", "Execute bash commands"),
            ("edit", "Make surgical edits"),
            ("write", "Create or overwrite files"),
        ])
    }

    fn skill(name: &str, description: &str, file_path: &str) -> Skill {
        Skill {
            name: name.to_owned(),
            description: description.to_owned(),
            file_path: file_path.to_owned(),
            base_dir: "/skills".to_owned(),
            source_info: create_synthetic_source_info(
                file_path,
                SyntheticSourceInfoOptions {
                    source: "test".to_owned(),
                    scope: None,
                    origin: None,
                    base_dir: Some("/skills".to_owned()),
                },
            ),
            disable_model_invocation: false,
        }
    }

    #[test]
    fn empty_tools_shows_none() {
        let prompt = build_system_prompt(&BuildSystemPromptOptions {
            selected_tools: Some(vec![]),
            context_files: Some(vec![]),
            skills: Some(vec![]),
            cwd: "/tmp/project".to_owned(),
            ..BuildSystemPromptOptions::default()
        });
        assert!(prompt.contains("Available tools:\n(none)"));
    }

    #[test]
    fn empty_tools_still_has_file_paths_guideline() {
        let prompt = build_system_prompt(&BuildSystemPromptOptions {
            selected_tools: Some(vec![]),
            context_files: Some(vec![]),
            skills: Some(vec![]),
            cwd: "/tmp/project".to_owned(),
            ..BuildSystemPromptOptions::default()
        });
        assert!(prompt.contains("Show file paths clearly"));
        assert!(prompt.contains("Be concise in your responses"));
    }

    #[test]
    fn default_tools_include_snippets_when_provided() {
        let prompt = build_system_prompt(&BuildSystemPromptOptions {
            tool_snippets: Some(default_tool_snippets()),
            context_files: Some(vec![]),
            skills: Some(vec![]),
            cwd: "/tmp/project".to_owned(),
            ..BuildSystemPromptOptions::default()
        });
        assert!(prompt.contains("- read: Read file contents"));
        assert!(prompt.contains("- bash: Execute bash commands"));
        assert!(prompt.contains("- edit: Make surgical edits"));
        assert!(prompt.contains("- write: Create or overwrite files"));
    }

    #[test]
    fn docs_section_always_present_with_absolute_paths() {
        let prompt = build_system_prompt(&BuildSystemPromptOptions {
            context_files: Some(vec![]),
            skills: Some(vec![]),
            cwd: "/tmp/project".to_owned(),
            ..BuildSystemPromptOptions::default()
        });
        assert!(prompt.contains(
            "- When reading pi docs or examples, resolve docs/... under Additional docs and examples/... under Examples, not the current working directory"
        ));
        assert!(prompt.contains("Pi documentation (read only when the user asks about pi itself"));
        let readme = path_display(&get_readme_path());
        let docs = path_display(&get_docs_path());
        let examples = path_display(&get_examples_path());
        assert!(prompt.contains(&format!("- Main documentation: {readme}")));
        assert!(prompt.contains(&format!("- Additional docs: {docs}")));
        assert!(prompt.contains(&format!(
            "- Examples: {examples} (extensions, custom tools, SDK)"
        )));
    }

    #[test]
    fn custom_tool_snippet_included_when_provided() {
        let prompt = build_system_prompt(&BuildSystemPromptOptions {
            selected_tools: Some(vec!["read".into(), "dynamic_tool".into()]),
            tool_snippets: Some(snippets(&[("dynamic_tool", "Run dynamic test behavior")])),
            context_files: Some(vec![]),
            skills: Some(vec![]),
            cwd: "/tmp/project".to_owned(),
            ..BuildSystemPromptOptions::default()
        });
        assert!(prompt.contains("- dynamic_tool: Run dynamic test behavior"));
    }

    #[test]
    fn custom_tool_without_snippet_omitted() {
        let prompt = build_system_prompt(&BuildSystemPromptOptions {
            selected_tools: Some(vec!["read".into(), "dynamic_tool".into()]),
            context_files: Some(vec![]),
            skills: Some(vec![]),
            cwd: "/tmp/project".to_owned(),
            ..BuildSystemPromptOptions::default()
        });
        assert!(!prompt.contains("dynamic_tool"));
        assert!(prompt.contains("Available tools:\n(none)"));
    }

    #[test]
    fn empty_snippet_string_omits_tool() {
        let prompt = build_system_prompt(&BuildSystemPromptOptions {
            selected_tools: Some(vec!["read".into()]),
            tool_snippets: Some(snippets(&[("read", "")])),
            cwd: "/tmp".to_owned(),
            ..BuildSystemPromptOptions::default()
        });
        assert!(prompt.contains("Available tools:\n(none)"));
    }

    #[test]
    fn prompt_guidelines_appended_and_ordered() {
        let prompt = build_system_prompt(&BuildSystemPromptOptions {
            selected_tools: Some(vec!["read".into(), "bash".into()]),
            prompt_guidelines: Some(vec!["Use dynamic_tool for project summaries.".into()]),
            context_files: Some(vec![]),
            skills: Some(vec![]),
            cwd: "/tmp/project".to_owned(),
            ..BuildSystemPromptOptions::default()
        });
        // bash without grep/find/ls → bash guideline first, then custom, then always.
        let bash_pos = prompt.find("- Use bash for file operations like ls, rg, find");
        let custom_pos = prompt.find("- Use dynamic_tool for project summaries.");
        let concise_pos = prompt.find("- Be concise in your responses");
        let paths_pos = prompt.find("- Show file paths clearly when working with files");
        assert!(bash_pos.is_some());
        assert!(custom_pos.is_some());
        assert!(concise_pos.is_some());
        assert!(paths_pos.is_some());
        assert!(bash_pos < custom_pos);
        assert!(custom_pos < concise_pos);
        assert!(concise_pos < paths_pos);
    }

    #[test]
    fn prompt_guidelines_dedupe_and_trim() {
        let prompt = build_system_prompt(&BuildSystemPromptOptions {
            selected_tools: Some(vec!["read".into()]),
            prompt_guidelines: Some(vec![
                "Use dynamic_tool for summaries.".into(),
                "  Use dynamic_tool for summaries.  ".into(),
                "   ".into(),
            ]),
            context_files: Some(vec![]),
            skills: Some(vec![]),
            cwd: "/tmp/project".to_owned(),
            ..BuildSystemPromptOptions::default()
        });
        let count = prompt.matches("- Use dynamic_tool for summaries.").count();
        assert_eq!(count, 1);
    }

    #[test]
    fn bash_guideline_skipped_when_search_tools_present() {
        let prompt = build_system_prompt(&BuildSystemPromptOptions {
            selected_tools: Some(vec![
                "bash".into(),
                "grep".into(),
                "find".into(),
                "ls".into(),
            ]),
            cwd: "/tmp".to_owned(),
            ..BuildSystemPromptOptions::default()
        });
        assert!(!prompt.contains("Use bash for file operations like ls, rg, find"));
    }

    #[test]
    fn custom_prompt_branch_order() {
        let prompt = build_system_prompt(&BuildSystemPromptOptions {
            custom_prompt: Some("CUSTOM BODY".into()),
            append: Some("APPEND TEXT".into()),
            context_files: Some(vec![AgentsFile {
                path: "/proj/AGENTS.md".into(),
                content: "Be nice".into(),
            }]),
            skills: Some(vec![skill("demo", "does demo", "/skills/demo/SKILL.md")]),
            selected_tools: Some(vec!["read".into()]),
            cwd: r"C:\Users\me\proj".into(),
            ..BuildSystemPromptOptions::default()
        });

        assert!(prompt.starts_with("CUSTOM BODY"));
        // No default identity / tools / docs in custom branch.
        assert!(!prompt.contains("You are an expert coding assistant"));
        assert!(!prompt.contains("Available tools:"));
        assert!(!prompt.contains("Pi documentation"));

        let custom_end = "CUSTOM BODY".len();
        let append_pos = prompt.find("\n\nAPPEND TEXT");
        let ctx_pos = prompt.find("<project_context>");
        let skills_pos = prompt.find("<available_skills>");
        let cwd_pos = prompt.find("\nCurrent working directory: C:/Users/me/proj");
        assert!(append_pos.is_some(), "append section missing");
        assert!(ctx_pos.is_some(), "project_context missing");
        assert!(skills_pos.is_some(), "skills section missing");
        assert!(cwd_pos.is_some(), "cwd line missing");
        assert!(append_pos.is_some_and(|p| p >= custom_end));
        assert!(append_pos < ctx_pos);
        assert!(ctx_pos < skills_pos);
        assert!(skills_pos < cwd_pos);
        assert!(prompt.contains(
            "<project_instructions path=\"/proj/AGENTS.md\">\nBe nice\n</project_instructions>"
        ));
    }

    #[test]
    fn custom_prompt_empty_string_uses_default_path() {
        let prompt = build_system_prompt(&BuildSystemPromptOptions {
            custom_prompt: Some(String::new()),
            cwd: "/tmp".to_owned(),
            ..BuildSystemPromptOptions::default()
        });
        assert!(prompt.contains("You are an expert coding assistant"));
    }

    #[test]
    fn skills_gated_on_read_default_path() {
        let skills = vec![skill("demo", "does demo", "/skills/demo/SKILL.md")];
        let with_read = build_system_prompt(&BuildSystemPromptOptions {
            selected_tools: Some(vec!["read".into()]),
            skills: Some(skills.clone()),
            cwd: "/tmp".to_owned(),
            ..BuildSystemPromptOptions::default()
        });
        assert!(with_read.contains("<available_skills>"));
        assert!(with_read.contains("<name>demo</name>"));

        let without_read = build_system_prompt(&BuildSystemPromptOptions {
            selected_tools: Some(vec!["bash".into()]),
            skills: Some(skills),
            cwd: "/tmp".to_owned(),
            ..BuildSystemPromptOptions::default()
        });
        assert!(!without_read.contains("<available_skills>"));
    }

    #[test]
    fn skills_gated_on_read_custom_path() {
        let skills = vec![skill("demo", "does demo", "/skills/demo/SKILL.md")];
        let none_selected = build_system_prompt(&BuildSystemPromptOptions {
            custom_prompt: Some("X".into()),
            selected_tools: None,
            skills: Some(skills.clone()),
            cwd: "/tmp".to_owned(),
            ..BuildSystemPromptOptions::default()
        });
        assert!(none_selected.contains("<available_skills>"));

        let no_read = build_system_prompt(&BuildSystemPromptOptions {
            custom_prompt: Some("X".into()),
            selected_tools: Some(vec!["bash".into()]),
            skills: Some(skills),
            cwd: "/tmp".to_owned(),
            ..BuildSystemPromptOptions::default()
        });
        assert!(!no_read.contains("<available_skills>"));
    }

    #[test]
    fn skills_xml_escaping_via_formatter() {
        let prompt = build_system_prompt(&BuildSystemPromptOptions {
            selected_tools: Some(vec!["read".into()]),
            skills: Some(vec![skill("a&b", "<x>", r#"/skills/"quoted"/SKILL.md"#)]),
            cwd: "/tmp".to_owned(),
            ..BuildSystemPromptOptions::default()
        });
        assert!(prompt.contains("<name>a&amp;b</name>"));
        assert!(prompt.contains("<description>&lt;x&gt;</description>"));
        // location escapes quotes
        assert!(prompt.contains(r"<location>/skills/&quot;quoted&quot;/SKILL.md</location>"));
    }

    #[test]
    fn project_context_path_and_content_unescaped() {
        // TypeScript inserts path/content raw into project_instructions.
        let prompt = build_system_prompt(&BuildSystemPromptOptions {
            custom_prompt: Some("BASE".into()),
            context_files: Some(vec![AgentsFile {
                path: r#"/p/"q".md"#.into(),
                content: "line <tag> & more".into(),
            }]),
            cwd: "/tmp".to_owned(),
            ..BuildSystemPromptOptions::default()
        });
        assert!(prompt.contains(
            r#"<project_instructions path="/p/"q".md">
line <tag> & more
</project_instructions>"#
        ));
    }

    #[test]
    fn cwd_backslash_normalization() {
        let prompt = build_system_prompt(&BuildSystemPromptOptions {
            custom_prompt: Some("X".into()),
            cwd: r"D:\work\repo".into(),
            ..BuildSystemPromptOptions::default()
        });
        assert!(prompt.ends_with("\nCurrent working directory: D:/work/repo"));
        assert!(!prompt.contains(r"D:\work\repo"));
    }

    #[test]
    fn default_section_order_append_context_skills_cwd() {
        let prompt = build_system_prompt(&BuildSystemPromptOptions {
            tool_snippets: Some(default_tool_snippets()),
            append: Some("APPEND".into()),
            context_files: Some(vec![AgentsFile {
                path: "/a.md".into(),
                content: "ctx".into(),
            }]),
            skills: Some(vec![skill("s", "d", "/s/SKILL.md")]),
            selected_tools: Some(vec![
                "read".into(),
                "bash".into(),
                "edit".into(),
                "write".into(),
            ]),
            cwd: "/cwd".into(),
            ..BuildSystemPromptOptions::default()
        });

        let identity = prompt.find("You are an expert coding assistant");
        let tools = prompt.find("Available tools:");
        let custom_tools = prompt
            .find("In addition to the tools above, you may have access to other custom tools");
        let guidelines = prompt.find("Guidelines:");
        let docs = prompt.find("Pi documentation");
        let append = prompt.find("\n\nAPPEND");
        let ctx = prompt.find("<project_context>");
        let skills = prompt.find("<available_skills>");
        let cwd = prompt.find("\nCurrent working directory: /cwd");

        assert!(identity.is_some());
        assert!(tools.is_some());
        assert!(custom_tools.is_some());
        assert!(guidelines.is_some());
        assert!(docs.is_some());
        assert!(append.is_some());
        assert!(ctx.is_some());
        assert!(skills.is_some());
        assert!(cwd.is_some());
        assert!(identity < tools);
        assert!(tools < custom_tools);
        assert!(custom_tools < guidelines);
        assert!(guidelines < docs);
        assert!(docs < append);
        assert!(append < ctx);
        assert!(ctx < skills);
        assert!(skills < cwd);
    }

    #[test]
    fn no_date_time_injection() {
        let prompt = build_system_prompt(&BuildSystemPromptOptions {
            cwd: "/tmp".into(),
            ..BuildSystemPromptOptions::default()
        });
        let lower = prompt.to_ascii_lowercase();
        assert!(!lower.contains("current date"));
        assert!(!lower.contains("today is"));
        assert!(!lower.contains("current time"));
    }
}
