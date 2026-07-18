//! Tool registry / activation impls.
//!
//! Ports `.references/pi/packages/coding-agent/src/core/agent-session.ts`
//! `getActiveToolNames`, `getAllTools`, `getToolDefinition`,
//! `setActiveToolsByName`, `_refreshToolRegistry`, and the tool-registry half
//! of `_buildRuntime`.
//!
//! Behaviour preserved from the TypeScript contract:
//! - Active tools are the live agent tool list; the registry is the superset
//!   of built-in + extension + SDK custom tools filtered by the allow /
//!   exclude lists.
//! - The registry preserves insertion order: built-ins first (in the order
//!   the constructor received them), then extension tools in registration
//!   order (first-wins on duplicate names). TypeScript `Map` iteration order
//!   is preserved using a `Vec`-backed registry.
//! - `set_active_tools_by_name` ignores unknown names, preserves order, and
//!   caches the validated names on `AgentSessionInner`.
//!
//! System-prompt rebuild: TypeScript `setActiveToolsByName` also calls
//! `_rebuildSystemPrompt(validToolNames)`. That rebuild needs context files,
//! skills, appends, and snippets owned by the (forthcoming) `system_prompt`
//! slice. This module deliberately does **not** mutate the system prompt; the
//! `system_prompt` slice installs the rebuild hook when it lands, preserving
//! the strict module ownership required by the foundation.
//!
//! `parse_skill_block` mirrors TypeScript `parseSkillBlock` so the
//! interactive mode and HTML / JSONL exporters can decode `<skill>` blocks
//! emitted by `expand_skill_invocation`.

use std::collections::HashSet;
use std::sync::Arc;

use pi_agent::AgentTool;

use super::AgentSession;

/// Public tool metadata returned by [`AgentSession::get_all_tools`].
#[derive(Clone, Debug, PartialEq)]
pub struct ToolInfo {
    /// Registered tool name.
    pub name: String,
    /// Human-readable description.
    pub description: String,
    /// JSON Schema for arguments.
    pub parameters: serde_json::Value,
}

/// Inputs for [`AgentSession::refresh_tool_registry`].
#[derive(Clone, Debug, Default)]
pub struct RefreshToolRegistryOptions {
    /// Override the active tool list (otherwise reuse the previous active
    /// list). Unknown names are dropped during `set_active_tools_by_name`.
    pub active_tool_names: Option<Vec<String>>,
    /// When `true`, all extension tools are appended to the active list on
    /// first build (TypeScript `includeAllExtensionTools`).
    pub include_all_extension_tools: bool,
}

impl AgentSession {
    /// Build the initial tool registry from the configured base tools and
    /// install the requested active set.
    ///
    /// Called once from [`super::AgentSession::new`]. Subsequent reloads go
    /// through [`Self::refresh_tool_registry`].
    pub(super) fn build_initial_tool_registry(
        &self,
        base_tools: Vec<Arc<dyn AgentTool>>,
        initial_active: Option<Vec<String>>,
        allowed: Option<Vec<String>>,
        excluded: Option<Vec<String>>,
    ) {
        {
            let mut inner = self.lock_inner();
            inner.allowed_tool_names = allowed.map(|names| names.into_iter().collect());
            inner.excluded_tool_names = excluded.map(|names| names.into_iter().collect());
            inner.base_tool_definitions = build_base_definitions(base_tools);
            inner.tool_registry.clear();
        }
        let opts = RefreshToolRegistryOptions {
            active_tool_names: initial_active,
            include_all_extension_tools: true,
        };
        self.refresh_tool_registry(&opts);
    }

    /// Refresh the tool registry from the current extension snapshot.
    ///
    /// Rebuilds:
    /// - `tool_registry`: built-in + extension + SDK custom tools (insertion
    ///   ordered, first-wins on duplicate names), filtered by allow/exclude.
    /// - `active_tool_names`: previous active list plus any newly-registered
    ///   allow/extension tools, filtered through the new registry.
    ///
    /// Finally calls [`Self::set_active_tools_by_name`] to apply the result to
    /// the agent state.
    pub fn refresh_tool_registry(&self, options: &RefreshToolRegistryOptions) {
        let runner = self.hooks.runner();
        let extension_tools = runner.get_all_registered_tools();
        let (base_definitions, allowed, excluded, previous_active, previous_registry_names) = {
            let inner = self.lock_inner();
            (
                inner.base_tool_definitions.clone(),
                inner.allowed_tool_names.clone(),
                inner.excluded_tool_names.clone(),
                self.agent_state_tool_names(),
                inner
                    .tool_registry
                    .iter()
                    .map(|entry| entry.name().to_owned())
                    .collect::<HashSet<String>>(),
            )
        };
        let is_allowed = |name: &str| {
            allowed.as_ref().is_none_or(|set| set.contains(name))
                && !excluded.as_ref().is_some_and(|set| set.contains(name))
        };

        // Built-in registry filtered by allow/exclude, preserving base order.
        let mut registry: Vec<Arc<dyn AgentTool>> =
            Vec::with_capacity(base_definitions.len().saturating_add(extension_tools.len()));
        let mut seen: HashSet<String> = HashSet::new();
        for tool in &base_definitions {
            let name = tool.name();
            if is_allowed(name) && seen.insert(name.to_owned()) {
                registry.push(Arc::clone(tool));
            }
        }
        // Extension + custom tools filtered by allow/exclude. First
        // registration wins for duplicate names; built-ins already in `seen`
        // take precedence. Extension tool ordering follows sorted name order
        // (HashMap iter is non-deterministic) so the wire ordering is stable.
        let mut extension_pairs: Vec<(String, Arc<dyn AgentTool>)> = extension_tools
            .into_iter()
            .filter(|(name, _)| is_allowed(name))
            .collect();
        extension_pairs.sort_by(|(a, _), (b, _)| a.cmp(b));
        for (name, tool) in extension_pairs {
            if seen.insert(name.clone()) {
                registry.push(tool);
            }
        }

        // Compute the next active list.
        let mut next_active: Vec<String> = match &options.active_tool_names {
            Some(names) => names.clone(),
            None => previous_active.clone(),
        };
        next_active.retain(|name| is_allowed(name));
        if let Some(allowed_set) = &allowed {
            for entry in &registry {
                if allowed_set.contains(entry.name()) {
                    next_active.push(entry.name().to_owned());
                }
            }
        } else if options.include_all_extension_tools {
            for entry in &registry {
                let name = entry.name();
                if !base_definitions.iter().any(|base| base.name() == name) {
                    next_active.push(name.to_owned());
                }
            }
        } else if options.active_tool_names.is_none() {
            for entry in &registry {
                let name = entry.name();
                if !previous_registry_names.contains(name) {
                    next_active.push(name.to_owned());
                }
            }
        }

        // Commit the registry before applying the active list so the
        // validation in `set_active_tools_by_name` can find every name.
        {
            let mut inner = self.lock_inner();
            inner.tool_registry = registry;
        }
        let deduped = dedup_preserve_order(next_active);
        self.set_active_tools_by_name(deduped);
    }

    /// Active tool names — the live agent tool list in registration order.
    #[must_use]
    pub fn get_active_tool_names(&self) -> Vec<String> {
        self.agent_state_tool_names()
    }

    /// All configured tools with name / description / parameter schema.
    ///
    /// Order matches the registry insertion order: built-ins first, then
    /// extension tools in alphabetical name order.
    #[must_use]
    pub fn get_all_tools(&self) -> Vec<ToolInfo> {
        self.lock_inner()
            .tool_registry
            .iter()
            .map(|tool| ToolInfo {
                name: tool.name().to_owned(),
                description: tool.description().to_owned(),
                parameters: tool.parameters().clone(),
            })
            .collect()
    }

    /// Look up a tool by name.
    #[must_use]
    pub fn get_tool(&self, name: &str) -> Option<Arc<dyn AgentTool>> {
        self.lock_inner()
            .tool_registry
            .iter()
            .find(|tool| tool.name() == name)
            .cloned()
    }

    /// Set the active tool set by name.
    ///
    /// Unknown names are ignored. Order is preserved. The agent tool list,
    /// the prepare-next-turn hook snapshot, and the cached active names are
    /// refreshed in lockstep.
    ///
    /// **System-prompt rebuild boundary**: the TypeScript reference also calls
    /// `_rebuildSystemPrompt(validToolNames)`. That rebuild needs context
    /// files, skills, and snippets owned by the `system_prompt` slice.
    pub fn set_active_tools_by_name(&self, tool_names: Vec<String>) {
        let registry = self.lock_inner().tool_registry.clone();
        let lookup = |name: &str| -> Option<Arc<dyn AgentTool>> {
            registry.iter().find(|tool| tool.name() == name).cloned()
        };
        let mut tools: Vec<Arc<dyn AgentTool>> = Vec::with_capacity(tool_names.len());
        let mut valid_names: Vec<String> = Vec::with_capacity(tool_names.len());
        let mut seen: HashSet<String> = HashSet::new();
        for name in tool_names {
            if seen.contains(&name) {
                continue;
            }
            if let Some(tool) = lookup(&name) {
                tools.push(tool);
                valid_names.push(name.clone());
                seen.insert(name);
            }
        }
        // Keep the hook snapshot synchronized so prepare_next_turn cannot
        // reinstall stale construction-time tools.
        self.hooks.set_tools(tools.clone());
        self.agent.set_tools(tools);
        {
            let mut inner = self.lock_inner();
            inner.active_tool_names = valid_names;
        }
    }

    /// Read the live agent tool names without holding `inner`.
    fn agent_state_tool_names(&self) -> Vec<String> {
        self.agent
            .state()
            .tools
            .iter()
            .map(|tool| tool.name().to_owned())
            .collect()
    }
}

// ---------------------------------------------------------------------------
// Skill block parsing
// ---------------------------------------------------------------------------

/// Parsed `<skill>` block extracted from user message text.
///
/// Mirrors TypeScript `ParsedSkillBlock`. Inverse of
/// [`crate::core::resources::skills::expand_skill_invocation`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ParsedSkillBlock {
    /// Skill name (`name="…"` attribute).
    pub name: String,
    /// Skill file path (`location="…"` attribute).
    pub location: String,
    /// Skill body between the open and close tags.
    pub content: String,
    /// Trailing user message after the block (`\n\n…`), trimmed.
    pub user_message: Option<String>,
}

/// Parse a `<skill name="…" location="…">…</skill>` block from message text.
///
/// Returns `None` when `text` does not start with a skill block. Mirrors the
/// TypeScript regex
/// `/^<skill name="([^"]+)" location="([^"]+)">\n([\s\S]*?)\n<\/skill>(?:\n\n([\s\S]+))?$/`.
#[must_use]
pub fn parse_skill_block(text: &str) -> Option<ParsedSkillBlock> {
    let prefix = "<skill name=\"";
    let after_name_open = text.strip_prefix(prefix)?;
    // The name attribute ends at `" location="`.
    let name_attr_close = "\" location=\"";
    let name_end = after_name_open.find(name_attr_close)?;
    let name = &after_name_open[..name_end];
    let after_location_open = &after_name_open[name_end + name_attr_close.len()..];
    // The location attribute ends at `">`.
    let location_end = after_location_open.find("\">")?;
    let location = &after_location_open[..location_end];
    let after_open_tag = &after_location_open[location_end + "\">".len()..];
    // Body must start with `\n` and end with `\n</skill>`.
    let body = after_open_tag.strip_prefix('\n')?;
    let close_tag = "\n</skill>";
    let body_end = body.find(close_tag)?;
    let content = &body[..body_end];
    let trailing = &body[body_end + close_tag.len()..];
    let user_message = match trailing.strip_prefix("\n\n") {
        Some(rest) => {
            let trimmed = rest.trim();
            if trimmed.is_empty() {
                None
            } else {
                Some(trimmed.to_owned())
            }
        }
        None => {
            // The regex anchors on `$`; any other trailing content means this
            // is not a skill block.
            if trailing.is_empty() {
                None
            } else {
                return None;
            }
        }
    };
    Some(ParsedSkillBlock {
        name: name.to_owned(),
        location: location.to_owned(),
        content: content.to_owned(),
        user_message,
    })
}

// ---------------------------------------------------------------------------
// Internals
// ---------------------------------------------------------------------------

/// Convert a list of base tools into an insertion-ordered vector.
///
/// Duplicate names keep the first entry (matches the TypeScript
/// `_baseToolDefinitions` Map construction, which also first-wins on dupes).
fn build_base_definitions(tools: Vec<Arc<dyn AgentTool>>) -> Vec<Arc<dyn AgentTool>> {
    let mut seen: HashSet<String> = HashSet::with_capacity(tools.len());
    let mut out: Vec<Arc<dyn AgentTool>> = Vec::with_capacity(tools.len());
    for tool in tools {
        if seen.insert(tool.name().to_owned()) {
            out.push(tool);
        }
    }
    out
}

/// Remove duplicates while preserving the first-seen order.
fn dedup_preserve_order(names: Vec<String>) -> Vec<String> {
    let mut seen: HashSet<String> = HashSet::with_capacity(names.len());
    let mut out = Vec::with_capacity(names.len());
    for name in names {
        if seen.contains(&name) {
            continue;
        }
        seen.insert(name.clone());
        out.push(name);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::Value;

    struct StubTool {
        name: String,
        description: String,
        parameters: Value,
    }

    impl StubTool {
        fn new_arc(name: &str) -> Arc<Self> {
            Arc::new(Self {
                name: name.to_owned(),
                description: format!("stub {name}"),
                parameters: serde_json::json!({ "type": "object" }),
            })
        }
    }

    impl AgentTool for StubTool {
        fn name(&self) -> &str {
            &self.name
        }
        fn label(&self) -> &str {
            &self.name
        }
        fn description(&self) -> &str {
            &self.description
        }
        fn parameters(&self) -> &Value {
            &self.parameters
        }
        fn validate_arguments(
            &self,
            args: &serde_json::Map<String, Value>,
        ) -> Result<serde_json::Map<String, Value>, pi_agent::ToolError> {
            Ok(args.clone())
        }
        fn execute(
            &self,
            _tool_call_id: &str,
            _args: serde_json::Map<String, Value>,
            _cancel: tokio_util::sync::CancellationToken,
            _updates: pi_agent::ToolUpdates,
        ) -> futures::future::BoxFuture<
            'static,
            Result<pi_agent::AgentToolResult, pi_agent::ToolError>,
        > {
            Box::pin(async { Ok(pi_agent::AgentToolResult::default()) })
        }
    }

    #[test]
    fn build_base_definitions_first_wins_and_preserves_order() {
        let a = StubTool::new_arc("read");
        let b = StubTool::new_arc("bash");
        let dup = StubTool::new_arc("read");
        let map = build_base_definitions(vec![a, b, dup]);
        assert_eq!(map.len(), 2);
        assert_eq!(map[0].name(), "read");
        assert_eq!(map[1].name(), "bash");
    }

    #[test]
    fn dedup_preserve_order_keeps_first() {
        let names = vec![
            "read".to_owned(),
            "bash".to_owned(),
            "read".to_owned(),
            "edit".to_owned(),
        ];
        assert_eq!(dedup_preserve_order(names), vec!["read", "bash", "edit"]);
    }

    #[test]
    fn tool_info_carries_name_description_parameters() {
        let tool = StubTool::new_arc("grep");
        let info = ToolInfo {
            name: tool.name().to_owned(),
            description: tool.description().to_owned(),
            parameters: tool.parameters().clone(),
        };
        assert_eq!(info.name, "grep");
        assert_eq!(info.description, "stub grep");
        assert_eq!(info.parameters, serde_json::json!({ "type": "object" }));
    }

    #[test]
    fn parses_simple_skill_block_without_user_message() -> Result<(), &'static str> {
        let text = "<skill name=\"commit\" location=\"/sk/commit.md\">\nbody\n</skill>";
        let parsed = parse_skill_block(text).ok_or("skill block should parse")?;
        assert_eq!(parsed.name, "commit");
        assert_eq!(parsed.location, "/sk/commit.md");
        assert_eq!(parsed.content, "body");
        assert!(parsed.user_message.is_none());
        Ok(())
    }

    #[test]
    fn parses_skill_block_with_user_message() -> Result<(), &'static str> {
        let text =
            "<skill name=\"commit\" location=\"/sk/commit.md\">\nbody\n</skill>\n\nfix the bug";
        let parsed = parse_skill_block(text).ok_or("skill block should parse")?;
        assert_eq!(parsed.user_message.as_deref(), Some("fix the bug"));
        Ok(())
    }

    #[test]
    fn returns_none_for_non_skill_text() {
        assert!(parse_skill_block("hello world").is_none());
        assert!(parse_skill_block("<other>").is_none());
    }

    #[test]
    fn returns_none_for_trailing_garbage() {
        let text = "<skill name=\"a\" location=\"b\">\nc\n</skill>extra";
        assert!(parse_skill_block(text).is_none());
    }

    #[test]
    fn parses_multi_line_body() -> Result<(), &'static str> {
        let text = "<skill name=\"a\" location=\"b\">\nline1\nline2\nline3\n</skill>";
        let parsed = parse_skill_block(text).ok_or("skill block should parse")?;
        assert_eq!(parsed.content, "line1\nline2\nline3");
        Ok(())
    }

    #[test]
    fn empty_user_message_after_separator_is_none() -> Result<(), &'static str> {
        let text = "<skill name=\"a\" location=\"b\">\nbody\n</skill>\n\n   ";
        let parsed = parse_skill_block(text).ok_or("skill block should parse")?;
        assert!(parsed.user_message.is_none());
        Ok(())
    }

    #[test]
    fn round_trips_with_expand_format() -> Result<(), &'static str> {
        // Match the exact format emitted by expand_skill_invocation.
        let text = "<skill name=\"commit\" location=\"/sk/commit.md\">\nReferences are relative to /sk.\n\nBody here\n</skill>";
        let parsed = parse_skill_block(text).ok_or("skill block should parse")?;
        assert_eq!(parsed.name, "commit");
        assert_eq!(
            parsed.content,
            "References are relative to /sk.\n\nBody here"
        );
        Ok(())
    }
}
