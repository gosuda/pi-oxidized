//! Serialized session data embedded in the HTML export template.
//!
//! Mirrors the TypeScript `SessionData` interface shape so the vendored
//! `template.js` viewer decodes it identically. Uses camelCase wire keys.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};
use serde_json::Value;

use super::super::sessions::entries::{SessionEntry, SessionHeader};

/// Pre-rendered HTML for a single custom tool call + result.
///
/// Serialized with camelCase keys matching `RenderedToolHtml` in
/// `.references/pi/packages/coding-agent/src/core/export-html/index.ts`.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RenderedToolHtml {
    /// HTML for the tool call block (absent when the tool has no call renderer).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub call_html: Option<String>,
    /// Collapsed result HTML.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub result_html_collapsed: Option<String>,
    /// Expanded result HTML.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub result_html_expanded: Option<String>,
}

/// Tool definition metadata embedded for client-side rendering context.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ToolInfo {
    /// Tool name.
    pub name: String,
    /// Human-readable description.
    pub description: String,
    /// JSON schema parameters.
    pub parameters: Value,
}

/// Session data payload base64-encoded into the HTML template.
///
/// Field order matches the TypeScript interface for deterministic output.
/// `entries` contains **all** file entries (full tree), not just the current
/// branch — the viewer needs the full tree for the sidebar.
#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SessionData {
    /// Session header (may be absent on malformed files).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub header: Option<SessionHeader>,
    /// All non-header entries serialized to JSON values (preserves unknown variants).
    pub entries: Vec<Value>,
    /// Current leaf entry id (drives default view).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub leaf_id: Option<String>,
    /// System prompt text (only from live `AgentState`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub system_prompt: Option<String>,
    /// Registered tool definitions (only from live `AgentState`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tools: Option<Vec<ToolInfo>>,
    /// Pre-rendered HTML for custom tool calls/results, keyed by tool call id.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rendered_tools: Option<BTreeMap<String, RenderedToolHtml>>,
}

impl SessionData {
    /// Build `SessionData` from session manager entries + optional live state.
    ///
    /// Entries are serialized to [`Value`] to preserve the exact wire shape
    /// (including unknown future variants and preserved extra fields).
    ///
    /// # Errors
    ///
    /// Returns a [`serde_json::Error`] when an entry cannot be serialized.
    pub fn from_entries(
        header: Option<&SessionHeader>,
        entries: &[&SessionEntry],
        leaf_id: Option<&str>,
        system_prompt: Option<&str>,
        tools: Option<&[ToolInfo]>,
        rendered_tools: Option<BTreeMap<String, RenderedToolHtml>>,
    ) -> Result<Self, serde_json::Error> {
        let mut values = Vec::with_capacity(entries.len());
        for entry in entries {
            values.push(serde_json::to_value(*entry)?);
        }

        Ok(Self {
            header: header.cloned(),
            entries: values,
            leaf_id: leaf_id.map(str::to_owned),
            system_prompt: system_prompt.map(str::to_owned),
            tools: tools.map(|t| t.to_vec()),
            rendered_tools: rendered_tools.filter(|m| !m.is_empty()),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn camel_case_serialization() {
        let data = SessionData {
            header: None,
            entries: vec![],
            leaf_id: Some("abc".to_owned()),
            system_prompt: Some("sys".to_owned()),
            tools: None,
            rendered_tools: None,
        };
        let json = serde_json::to_value(&data).unwrap();
        assert_eq!(json["leafId"], "abc");
        assert_eq!(json["systemPrompt"], "sys");
        assert!(json.get("renderedTools").is_none());
        assert!(json.get("tools").is_none());
    }

    #[test]
    fn rendered_tools_camel_case() {
        let rt = RenderedToolHtml {
            call_html: Some("<b>".to_owned()),
            result_html_collapsed: Some("c".to_owned()),
            result_html_expanded: Some("e".to_owned()),
        };
        let json = serde_json::to_value(&rt).unwrap();
        assert_eq!(json["callHtml"], "<b>");
        assert_eq!(json["resultHtmlCollapsed"], "c");
        assert_eq!(json["resultHtmlExpanded"], "e");
    }

    #[test]
    fn empty_rendered_tools_filtered() {
        let data = SessionData {
            header: None,
            entries: vec![],
            leaf_id: None,
            system_prompt: None,
            tools: None,
            rendered_tools: Some(BTreeMap::new()),
        };
        let json = serde_json::to_value(&data).unwrap();
        assert!(json.get("renderedTools").is_none());
    }
}
