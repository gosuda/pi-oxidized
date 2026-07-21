//! Self-contained HTML session export compatible with the TypeScript viewer.

pub mod ansi_to_html;

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};

use base64::Engine as _;
use indexmap::IndexMap;
use pi_agent::{AgentState, AgentStateSnapshot};
use pi_ai::{AssistantContent, Message, ToolResultContent};
use pi_tui::terminal::probe::TerminalTheme;
use serde::Serialize;
use serde_json::Value;
use thiserror::Error;

use crate::core::config::{APP_NAME, PathInputOptions, normalize_path, resolve_path};
use crate::core::sessions::{SessionEntry, SessionError, SessionHeader, SessionManager};
use crate::core::settings::ThemeMode;
use crate::modes::interactive::theme::{
    BUILT_IN_THEME_NAMES, ColorMode, ResolvedTheme, Rgb, ThemeSlotValue, bg_slot_names, dark,
    fg_slot_names, load_or_dark, resolve_active_theme,
};

const TEMPLATE_HTML: &str = include_str!("../../../assets/export-html/template.html");
const TEMPLATE_CSS: &str = include_str!("../../../assets/export-html/template.css");
const TEMPLATE_JS: &str = include_str!("../../../assets/export-html/template.js");
const MARKED_JS: &str = include_str!("../../../assets/export-html/vendor/marked.min.js");
const HIGHLIGHT_JS: &str = include_str!("../../../assets/export-html/vendor/highlight.min.js");
const DARK_THEME: &str = include_str!("../../../assets/export-html/vendor/dark.json");
const LIGHT_THEME: &str = include_str!("../../../assets/export-html/vendor/light.json");
const TEMPLATE_RENDERED_TOOLS: [&str; 5] = ["bash", "read", "write", "edit", "ls"];

/// A tool definition embedded into an exported session.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct ToolInfo {
    /// Registered tool name.
    pub name: String,
    /// Human-readable description.
    pub description: String,
    /// JSON Schema for arguments.
    pub parameters: Value,
}

/// Optional live-agent data unavailable when exporting an arbitrary file.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct SessionExportState {
    /// System prompt active for the session.
    pub system_prompt: String,
    /// Tools active for the session.
    pub tools: Vec<ToolInfo>,
}

impl SessionExportState {
    /// Capture the exportable subset of mutable agent state.
    #[must_use]
    pub fn from_agent_state(state: &AgentState) -> Self {
        Self {
            system_prompt: state.system_prompt.clone(),
            tools: state
                .tools
                .iter()
                .map(|tool| ToolInfo {
                    name: tool.name().to_owned(),
                    description: tool.description().to_owned(),
                    parameters: tool.parameters().clone(),
                })
                .collect(),
        }
    }

    /// Capture the exportable subset of an immutable state snapshot.
    #[must_use]
    pub fn from_agent_snapshot(state: &AgentStateSnapshot) -> Self {
        Self {
            system_prompt: state.system_prompt.clone(),
            tools: state
                .tools
                .iter()
                .map(|tool| ToolInfo {
                    name: tool.name().to_owned(),
                    description: tool.description().to_owned(),
                    parameters: tool.parameters().clone(),
                })
                .collect(),
        }
    }
}

/// HTML fragments returned by a custom tool-result renderer.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct RenderedResult {
    /// Collapsed result fragment.
    pub collapsed: Option<String>,
    /// Expanded result fragment.
    pub expanded: Option<String>,
}

/// Renderer seam for extension-defined tools.
pub trait ToolHtmlRenderer: Send + Sync {
    /// Render a custom tool call, or return `None` to use the generic viewer.
    fn render_call(&self, tool_call_id: &str, tool_name: &str, arguments: &Value)
    -> Option<String>;

    /// Render a custom tool result, or return `None` to use the generic viewer.
    fn render_result(
        &self,
        tool_call_id: &str,
        tool_name: &str,
        result: &[ToolResultContent],
        details: Option<&Value>,
        is_error: bool,
    ) -> Option<RenderedResult>;
}

/// Pre-rendered custom-tool fragments keyed by tool-call id.
#[derive(Clone, Debug, Default, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct RenderedToolHtml {
    /// Tool-call fragment.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub call_html: Option<String>,
    /// Collapsed result fragment.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub result_html_collapsed: Option<String>,
    /// Expanded result fragment.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub result_html_expanded: Option<String>,
}

/// Fully resolved colors used by the HTML viewer.
#[derive(Clone, Debug, PartialEq)]
pub struct ExportTheme {
    colors: Vec<(String, String)>,
    page_background: Option<String>,
    card_background: Option<String>,
    info_background: Option<String>,
}

#[derive(serde::Deserialize)]
struct ThemeDocument {
    #[serde(default)]
    vars: IndexMap<String, ThemeColor>,
    #[serde(default)]
    colors: IndexMap<String, ThemeColor>,
    #[serde(default)]
    export: ThemeExport,
}

#[derive(Clone, serde::Deserialize)]
#[serde(untagged)]
enum ThemeColor {
    Text(String),
    Ansi(u16),
}

#[derive(Default, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
struct ThemeExport {
    #[serde(rename = "pageBg")]
    page: Option<ThemeColor>,
    #[serde(rename = "cardBg")]
    card: Option<ThemeColor>,
    #[serde(rename = "infoBg")]
    info: Option<ThemeColor>,
}

impl ExportTheme {
    /// Resolve a pi theme JSON document to CSS-compatible colors.
    ///
    /// # Errors
    ///
    /// Returns a JSON error when the document is malformed.
    pub fn from_json(source: &str, light: bool) -> Result<Self, serde_json::Error> {
        let document: ThemeDocument = serde_json::from_str(source)?;
        let default_text = if light { "#000000" } else { "#e5e5e7" };
        let colors = document
            .colors
            .iter()
            .map(|(name, color)| {
                (
                    name.clone(),
                    resolve_theme_color(color, &document.vars, default_text),
                )
            })
            .collect();
        Ok(Self {
            colors,
            page_background: document
                .export
                .page
                .as_ref()
                .map(|value| resolve_theme_color(value, &document.vars, ""))
                .filter(|value| !value.is_empty()),
            card_background: document
                .export
                .card
                .as_ref()
                .map(|value| resolve_theme_color(value, &document.vars, ""))
                .filter(|value| !value.is_empty()),
            info_background: document
                .export
                .info
                .as_ref()
                .map(|value| resolve_theme_color(value, &document.vars, ""))
                .filter(|value| !value.is_empty()),
        })
    }

    /// Build export colors from a fully resolved interactive theme.
    ///
    /// Slot values come from the resolved fg/bg tables (same CSS variable
    /// names as the theme schema). Page/card/info backgrounds are left unset
    /// so [`generate_html`] can derive them from `userMessageBg`, matching
    /// themes that omit an `export` block.
    #[must_use]
    pub fn from_resolved(theme: &ResolvedTheme) -> Self {
        let default_text = if is_light_theme_name(theme.name.as_ref()) {
            "#000000"
        } else {
            "#e5e5e7"
        };
        let mut colors = Vec::with_capacity(fg_slot_names().len() + bg_slot_names().len());
        for (slot, name) in fg_slot_names() {
            let value = if theme.is_fg_empty(*slot) {
                default_text.to_owned()
            } else {
                slot_color_to_hex(theme.fg_value(*slot), default_text)
            };
            colors.push(((*name).to_owned(), value));
        }
        for (slot, name) in bg_slot_names() {
            let value = if theme.is_bg_empty(*slot) {
                String::new()
            } else {
                slot_color_to_hex(theme.bg_value(*slot), "")
            };
            colors.push(((*name).to_owned(), value));
        }
        Self {
            colors,
            page_background: None,
            card_background: None,
            info_background: None,
        }
    }

    /// Resolve a built-in theme name through the interactive theme interns.
    ///
    /// Unknown or missing names fall back to the default dark theme. For the
    /// `dark`/`light` family, vendor export page/card/info colors are overlaid
    /// so HTML export keeps the explicit backgrounds shipped for those two.
    #[must_use]
    pub fn built_in(name: Option<&str>) -> Self {
        let resolved = match name {
            Some(name) if BUILT_IN_THEME_NAMES.contains(&name) => {
                load_or_dark(name, ColorMode::Truecolor)
            }
            _ => dark(),
        };
        let mut theme = Self::from_resolved(&resolved);
        overlay_vendor_export_backgrounds(&mut theme, resolved.name.as_ref());
        theme
    }
}

fn is_light_theme_name(name: &str) -> bool {
    name == "light" || name.ends_with("-light")
}

fn rgb_to_hex(Rgb(red, green, blue): Rgb) -> String {
    format!("#{red:02x}{green:02x}{blue:02x}")
}

fn slot_color_to_hex(value: ThemeSlotValue, default_text: &str) -> String {
    match value {
        ThemeSlotValue::Empty => default_text.to_owned(),
        ThemeSlotValue::Indexed(index) => ansi_256_to_hex(u16::from(index)),
        ThemeSlotValue::Rgb(rgb) => rgb_to_hex(rgb),
    }
}

fn overlay_vendor_export_backgrounds(theme: &mut ExportTheme, name: &str) {
    let (source, light) = match name {
        "light" => (LIGHT_THEME, true),
        "dark" => (DARK_THEME, false),
        _ => return,
    };
    let Ok(vendor) = ExportTheme::from_json(source, light) else {
        return;
    };
    theme.page_background = vendor.page_background;
    theme.card_background = vendor.card_background;
    theme.info_background = vendor.info_background;
}

/// Headless export resolution: Auto/pair settings pick the dark member.
#[must_use]
pub fn resolve_export_theme(raw: Option<&str>, mode: ThemeMode) -> ExportTheme {
    let resolved = resolve_active_theme(raw, mode, TerminalTheme::Dark, ColorMode::Truecolor);
    let mut theme = ExportTheme::from_resolved(&resolved);
    overlay_vendor_export_backgrounds(&mut theme, resolved.name.as_ref());
    theme
}

fn resolve_theme_color(
    color: &ThemeColor,
    variables: &IndexMap<String, ThemeColor>,
    default_text: &str,
) -> String {
    let mut current = color;
    for _ in 0..variables.len().saturating_add(1) {
        match current {
            ThemeColor::Ansi(index) => return ansi_256_to_hex(*index),
            ThemeColor::Text(text) if text.is_empty() => return default_text.to_owned(),
            ThemeColor::Text(text) => match variables.get(text) {
                Some(next) => current = next,
                None => return text.clone(),
            },
        }
    }
    match current {
        ThemeColor::Ansi(index) => ansi_256_to_hex(*index),
        ThemeColor::Text(text) => text.clone(),
    }
}

fn ansi_256_to_hex(index: u16) -> String {
    const BASIC: [&str; 16] = [
        "#000000", "#800000", "#008000", "#808000", "#000080", "#800080", "#008080", "#c0c0c0",
        "#808080", "#ff0000", "#00ff00", "#ffff00", "#0000ff", "#ff00ff", "#00ffff", "#ffffff",
    ];
    let index = index.min(255);
    if index < 16 {
        return BASIC[usize::from(index)].to_owned();
    }
    if index < 232 {
        let cube = index - 16;
        let part = |value: u16| if value == 0 { 0 } else { 55 + value * 40 };
        return format!(
            "#{:02x}{:02x}{:02x}",
            part(cube / 36),
            part((cube % 36) / 6),
            part(cube % 6)
        );
    }
    let gray = 8 + (index - 232) * 10;
    format!("#{gray:02x}{gray:02x}{gray:02x}")
}

/// HTML export options.
#[derive(Default)]
pub struct ExportOptions<'a> {
    /// Output path; a pi-compatible basename is generated when absent.
    pub output_path: Option<PathBuf>,
    /// Built-in theme name (any of the ten built-in themes). Unknown names use `dark`.
    pub theme_name: Option<String>,
    /// Resolved custom theme, taking precedence over `theme_name`.
    pub theme: Option<ExportTheme>,
    /// Optional custom-tool renderer.
    pub tool_renderer: Option<&'a dyn ToolHtmlRenderer>,
}

impl ExportOptions<'_> {
    /// Build options from an optional output-path string.
    #[must_use]
    pub fn with_output_path(output_path: Option<&str>) -> Self {
        Self {
            output_path: output_path.map(PathBuf::from),
            ..Self::default()
        }
    }
}

/// HTML export failures.
#[derive(Debug, Error)]
pub enum ExportError {
    /// A current in-memory session has no file to export.
    #[error("Cannot export in-memory session to HTML")]
    InMemory,
    /// Deferred persistence has not created the session file yet.
    #[error("Nothing to export yet - start a conversation first")]
    Empty,
    /// An arbitrary input path does not exist.
    #[error("File not found: {0}")]
    FileNotFound(String),
    /// Session loading failed.
    #[error(transparent)]
    Session(#[from] SessionError),
    /// JSON encoding failed.
    #[error(transparent)]
    Json(#[from] serde_json::Error),
    /// Output could not be written.
    #[error("Failed to write HTML export {path}: {source}")]
    Write {
        /// Target path.
        path: String,
        /// Underlying filesystem error.
        source: std::io::Error,
    },
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct SessionData<'a> {
    #[serde(skip_serializing_if = "Option::is_none")]
    header: Option<&'a SessionHeader>,
    entries: Vec<&'a SessionEntry>,
    leaf_id: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    system_prompt: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    tools: Option<&'a [ToolInfo]>,
    #[serde(skip_serializing_if = "Option::is_none")]
    rendered_tools: Option<BTreeMap<String, RenderedToolHtml>>,
}

fn is_template_rendered_tool(name: &str) -> bool {
    TEMPLATE_RENDERED_TOOLS.contains(&name)
}

fn pre_render_custom_tools(
    entries: &[&SessionEntry],
    renderer: &dyn ToolHtmlRenderer,
) -> BTreeMap<String, RenderedToolHtml> {
    let mut rendered_custom_tools: BTreeMap<String, RenderedToolHtml> = BTreeMap::new();
    for entry in entries {
        let SessionEntry::Message(message_entry) = entry else {
            continue;
        };
        let Some(message) = message_entry.message.as_llm() else {
            continue;
        };
        match message {
            Message::Assistant(message) => {
                for block in &message.content {
                    let AssistantContent::ToolCall(call) = block else {
                        continue;
                    };
                    if is_template_rendered_tool(&call.name) {
                        continue;
                    }
                    let arguments = Value::Object(call.arguments.clone());
                    if let Some(call_html) = renderer.render_call(&call.id, &call.name, &arguments)
                    {
                        rendered_custom_tools
                            .entry(call.id.clone())
                            .or_default()
                            .call_html = Some(call_html);
                    }
                }
            }
            Message::ToolResult(result) => {
                let existing = rendered_custom_tools.contains_key(&result.tool_call_id);
                if !existing && is_template_rendered_tool(&result.tool_name) {
                    continue;
                }
                if let Some(fragment) = renderer.render_result(
                    &result.tool_call_id,
                    &result.tool_name,
                    &result.content,
                    result.details.as_ref(),
                    result.is_error,
                ) {
                    let item = rendered_custom_tools
                        .entry(result.tool_call_id.clone())
                        .or_default();
                    item.result_html_collapsed = fragment.collapsed;
                    item.result_html_expanded = fragment.expanded;
                }
            }
            Message::User(_) => {}
        }
    }
    rendered_custom_tools
}

fn parse_rgb(color: &str) -> Option<(u8, u8, u8)> {
    if let Some(hex) = color.strip_prefix('#').filter(|value| value.len() == 6) {
        return Some((
            u8::from_str_radix(&hex[0..2], 16).ok()?,
            u8::from_str_radix(&hex[2..4], 16).ok()?,
            u8::from_str_radix(&hex[4..6], 16).ok()?,
        ));
    }
    let raw = color.strip_prefix("rgb")?.trim();
    let raw = raw.strip_prefix('(')?.strip_suffix(')')?;
    let mut parts = raw.split(',').map(str::trim);
    let red = parts.next()?.parse().ok()?;
    let green = parts.next()?.parse().ok()?;
    let blue = parts.next()?.parse().ok()?;
    if parts.next().is_some() {
        return None;
    }
    Some((red, green, blue))
}

fn relative_luminance(red: u8, green: u8, blue: u8) -> f64 {
    let linear = |component: u8| {
        let value = f64::from(component) / 255.0;
        if value <= 0.039_28 {
            value / 12.92
        } else {
            ((value + 0.055) / 1.055).powf(2.4)
        }
    };
    0.2126 * linear(red) + 0.7152 * linear(green) + 0.0722 * linear(blue)
}

fn bounded_rounded_u8(value: f64) -> u8 {
    let value = value.round().clamp(0.0, 255.0);
    if value.is_nan() {
        return 0;
    }

    let mut lower = 0_u8;
    let mut upper = u8::MAX;
    while lower < upper {
        let midpoint = lower + (upper - lower) / 2;
        if f64::from(midpoint) < value {
            lower = midpoint + 1;
        } else {
            upper = midpoint;
        }
    }
    lower
}

fn adjust_brightness(color: &str, factor: f64) -> String {
    let Some((red, green, blue)) = parse_rgb(color) else {
        return color.to_owned();
    };
    let adjust = |value: u8| bounded_rounded_u8(f64::from(value) * factor);
    format!("rgb({}, {}, {})", adjust(red), adjust(green), adjust(blue))
}

fn derived_export_colors(base: &str) -> (String, String, String) {
    let Some((red, green, blue)) = parse_rgb(base) else {
        return (
            "rgb(24, 24, 30)".to_owned(),
            "rgb(30, 30, 36)".to_owned(),
            "rgb(60, 55, 40)".to_owned(),
        );
    };
    if relative_luminance(red, green, blue) > 0.5 {
        (
            adjust_brightness(base, 0.96),
            base.to_owned(),
            format!(
                "rgb({}, {}, {})",
                red.saturating_add(10),
                green.saturating_add(5),
                blue.saturating_sub(20)
            ),
        )
    } else {
        (
            adjust_brightness(base, 0.7),
            adjust_brightness(base, 0.85),
            format!(
                "rgb({}, {}, {})",
                red.saturating_add(20),
                green.saturating_add(15),
                blue
            ),
        )
    }
}

fn generate_html(data: &SessionData<'_>, theme: &ExportTheme) -> Result<String, ExportError> {
    let user_background = theme
        .colors
        .iter()
        .find(|(name, _)| name == "userMessageBg")
        .map_or("#343541", |(_, value)| value.as_str());
    let derived = derived_export_colors(user_background);
    let page = theme.page_background.as_ref().unwrap_or(&derived.0);
    let card = theme.card_background.as_ref().unwrap_or(&derived.1);
    let info = theme.info_background.as_ref().unwrap_or(&derived.2);

    let mut theme_variables = String::new();
    for (index, (name, value)) in theme.colors.iter().enumerate() {
        if index > 0 {
            theme_variables.push_str("\n      ");
        }
        theme_variables.push_str("--");
        theme_variables.push_str(name);
        theme_variables.push_str(": ");
        theme_variables.push_str(value);
        theme_variables.push(';');
    }
    for (name, value) in [
        ("exportPageBg", page),
        ("exportCardBg", card),
        ("exportInfoBg", info),
    ] {
        if !theme_variables.is_empty() {
            theme_variables.push_str("\n      ");
        }
        theme_variables.push_str("--");
        theme_variables.push_str(name);
        theme_variables.push_str(": ");
        theme_variables.push_str(value);
        theme_variables.push(';');
    }

    let css = TEMPLATE_CSS
        .replacen("{{THEME_VARS}}", &theme_variables, 1)
        .replacen("{{BODY_BG}}", page, 1)
        .replacen("{{CONTAINER_BG}}", card, 1)
        .replacen("{{INFO_BG}}", info, 1);
    let encoded = base64::engine::general_purpose::STANDARD.encode(serde_json::to_vec(data)?);
    Ok(TEMPLATE_HTML
        .replacen("{{CSS}}", &css, 1)
        .replacen("{{JS}}", TEMPLATE_JS, 1)
        .replacen("{{SESSION_DATA}}", &encoded, 1)
        .replacen("{{MARKED_JS}}", MARKED_JS, 1)
        .replacen("{{HIGHLIGHT_JS}}", HIGHLIGHT_JS, 1))
}

fn default_output_path(session_file: &str) -> PathBuf {
    let name = Path::new(session_file)
        .file_name()
        .and_then(|value| value.to_str())
        .unwrap_or(session_file);
    let stem = name.strip_suffix(".jsonl").unwrap_or(name);
    PathBuf::from(format!("{APP_NAME}-session-{stem}.html"))
}

fn write_export(path: &Path, html: &str) -> Result<String, ExportError> {
    fs::write(path, html).map_err(|source| ExportError::Write {
        path: path.to_string_lossy().into_owned(),
        source,
    })?;
    Ok(path.to_string_lossy().into_owned())
}

/// Export a current session, including live system-prompt and tool metadata.
///
/// # Errors
///
/// Returns exact compatibility errors for in-memory/deferred sessions, or an
/// encoding, session, or output error.
pub fn export_session_to_html(
    session: &SessionManager,
    state: Option<&SessionExportState>,
    options: ExportOptions<'_>,
) -> Result<String, ExportError> {
    let session_file = session.get_session_file().ok_or(ExportError::InMemory)?;
    if !Path::new(session_file).exists() {
        return Err(ExportError::Empty);
    }
    let entries = session.get_entries();
    let rendered_tools = options
        .tool_renderer
        .map(|renderer| pre_render_custom_tools(&entries, renderer))
        .filter(|rendered| !rendered.is_empty());
    let data = SessionData {
        header: session.get_header(),
        entries,
        leaf_id: session.get_leaf_id(),
        system_prompt: state.map(|value| value.system_prompt.as_str()),
        tools: state.map(|value| value.tools.as_slice()),
        rendered_tools,
    };
    let theme = options
        .theme
        .unwrap_or_else(|| ExportTheme::built_in(options.theme_name.as_deref()));
    let html = generate_html(&data, &theme)?;
    let output = options
        .output_path
        .unwrap_or_else(|| default_output_path(session_file));
    let normalized = normalize_path(
        &output.to_string_lossy(),
        PathInputOptions::new().trim(false),
    );
    write_export(&normalized, &html)
}

/// Export an arbitrary session file without live agent state.
///
/// # Errors
///
/// Returns `File not found: <resolved path>` for a missing input, or a session,
/// encoding, or output error.
pub fn export_from_file(
    input_path: &str,
    options: ExportOptions<'_>,
) -> Result<String, ExportError> {
    let input = resolve_path(input_path);
    if !input.exists() {
        return Err(ExportError::FileNotFound(
            input.to_string_lossy().into_owned(),
        ));
    }
    let session = SessionManager::open(&input.to_string_lossy(), None, None)?;
    let entries = session.get_entries();
    let rendered_tools = options
        .tool_renderer
        .map(|renderer| pre_render_custom_tools(&entries, renderer))
        .filter(|rendered| !rendered.is_empty());
    let data = SessionData {
        header: session.get_header(),
        entries,
        leaf_id: session.get_leaf_id(),
        system_prompt: None,
        tools: None,
        rendered_tools,
    };
    let theme = options
        .theme
        .unwrap_or_else(|| ExportTheme::built_in(options.theme_name.as_deref()));
    let html = generate_html(&data, &theme)?;
    let output = options
        .output_path
        .unwrap_or_else(|| default_output_path(input.to_string_lossy().as_ref()));
    let normalized = normalize_path(
        &output.to_string_lossy(),
        PathInputOptions::new().trim(false),
    );
    write_export(&normalized, &html)
}

#[cfg(test)]
mod tests {
    use super::*;
    use base64::engine::general_purpose::STANDARD;
    use serde_json::json;
    use tempfile::tempdir;

    fn fixture(root: &Path) -> Result<PathBuf, std::io::Error> {
        let path = root.join("fixture.jsonl");
        fs::write(
            &path,
            concat!(
                "{\"type\":\"session\",\"version\":3,\"id\":\"session-id\",\"timestamp\":\"2026-01-01T00:00:00.000Z\",\"cwd\":\"/tmp\"}\n",
                "{\"type\":\"message\",\"id\":\"a1\",\"parentId\":null,\"timestamp\":\"2026-01-01T00:00:01.000Z\",\"message\":{\"role\":\"user\",\"content\":\"<hello>&\",\"timestamp\":1}}\n"
            ),
        )?;
        Ok(path)
    }

    fn embedded_data(html: &str) -> Result<Value, Box<dyn std::error::Error>> {
        let marker = "<script id=\"session-data\" type=\"application/json\">";
        let start = html.find(marker).ok_or("session data marker missing")? + marker.len();
        let end = html[start..]
            .find("</script>")
            .ok_or("session data terminator missing")?
            + start;
        let decoded = STANDARD.decode(&html[start..end])?;
        Ok(serde_json::from_slice(&decoded)?)
    }

    #[test]
    fn arbitrary_file_export_embeds_full_session_data_and_assets()
    -> Result<(), Box<dyn std::error::Error>> {
        let root = tempdir()?;
        let input = fixture(root.path())?;
        let output = root.path().join("out.html");
        export_from_file(
            &input.to_string_lossy(),
            ExportOptions {
                output_path: Some(output.clone()),
                theme_name: Some("light".to_owned()),
                ..ExportOptions::default()
            },
        )?;
        let html = fs::read_to_string(output)?;
        assert!(html.contains("marked v18.0.5"));
        assert!(html.contains("Highlight.js v11.9.0"));
        assert!(html.contains("--exportPageBg: #f8f8f8;"));
        assert!(!html.contains("{{SESSION_DATA}}"));
        let data = embedded_data(&html)?;
        assert_eq!(data["header"]["version"], 3);
        assert_eq!(data["entries"].as_array().map(Vec::len), Some(1));
        assert_eq!(data["leafId"], "a1");
        assert!(data.get("systemPrompt").is_none());
        assert!(data.get("tools").is_none());
        assert_eq!(data["entries"][0]["message"]["content"], "<hello>&");
        Ok(())
    }

    #[test]
    fn missing_file_in_memory_and_deferred_session_errors_are_exact()
    -> Result<(), Box<dyn std::error::Error>> {
        let root = tempdir()?;
        let missing = root.path().join("missing.jsonl");
        let error = export_from_file(&missing.to_string_lossy(), ExportOptions::default())
            .err()
            .ok_or("missing export error")?;
        assert_eq!(
            error.to_string(),
            format!("File not found: {}", missing.display())
        );

        let memory = SessionManager::in_memory(Some(&root.path().to_string_lossy()), None)?;
        let error = export_session_to_html(&memory, None, ExportOptions::default())
            .err()
            .ok_or("missing in-memory error")?;
        assert_eq!(error.to_string(), "Cannot export in-memory session to HTML");

        let persisted = SessionManager::create(
            &root.path().to_string_lossy(),
            Some(&root.path().join("sessions").to_string_lossy()),
            None,
        )?;
        let error = export_session_to_html(&persisted, None, ExportOptions::default())
            .err()
            .ok_or("missing deferred-session error")?;
        assert_eq!(
            error.to_string(),
            "Nothing to export yet - start a conversation first"
        );
        Ok(())
    }

    struct Renderer;

    impl ToolHtmlRenderer for Renderer {
        fn render_call(
            &self,
            tool_call_id: &str,
            tool_name: &str,
            _arguments: &Value,
        ) -> Option<String> {
            Some(format!("&lt;{tool_name}:{tool_call_id}&gt;"))
        }

        fn render_result(
            &self,
            _tool_call_id: &str,
            _tool_name: &str,
            _result: &[ToolResultContent],
            _details: Option<&Value>,
            _is_error: bool,
        ) -> Option<RenderedResult> {
            Some(RenderedResult {
                collapsed: Some("collapsed".to_owned()),
                expanded: Some("expanded".to_owned()),
            })
        }
    }

    #[test]
    fn custom_tools_are_pre_rendered_but_builtins_stay_client_side()
    -> Result<(), Box<dyn std::error::Error>> {
        let root = tempdir()?;
        let input = root.path().join("tools.jsonl");
        fs::write(
            &input,
            concat!(
                "{\"type\":\"session\",\"version\":3,\"id\":\"s\",\"timestamp\":\"2026-01-01T00:00:00.000Z\",\"cwd\":\"/tmp\"}\n",
                "{\"type\":\"message\",\"id\":\"a\",\"parentId\":null,\"timestamp\":\"2026-01-01T00:00:01.000Z\",\"message\":{\"role\":\"assistant\",\"content\":[{\"type\":\"toolCall\",\"id\":\"custom-id\",\"name\":\"custom\",\"arguments\":{}},{\"type\":\"toolCall\",\"id\":\"read-id\",\"name\":\"read\",\"arguments\":{}}],\"api\":\"x\",\"provider\":\"x\",\"model\":\"x\",\"usage\":{\"input\":0,\"output\":0,\"cacheRead\":0,\"cacheWrite\":0,\"totalTokens\":0,\"cost\":{\"input\":0,\"output\":0,\"cacheRead\":0,\"cacheWrite\":0,\"total\":0}},\"stopReason\":\"toolUse\",\"timestamp\":1}}\n"
            ),
        )?;
        let output = root.path().join("tools.html");
        export_from_file(
            &input.to_string_lossy(),
            ExportOptions {
                output_path: Some(output.clone()),
                tool_renderer: Some(&Renderer),
                ..ExportOptions::default()
            },
        )?;
        let data = embedded_data(&fs::read_to_string(output)?)?;
        assert_eq!(
            data["renderedTools"]["custom-id"]["callHtml"],
            json!("&lt;custom:custom-id&gt;")
        );
        assert!(data["renderedTools"].get("read-id").is_none());
        Ok(())
    }

    fn css_var(theme: &ExportTheme, name: &str) -> Option<String> {
        theme
            .colors
            .iter()
            .find(|(key, _)| key == name)
            .map(|(_, value)| value.clone())
    }

    #[test]
    fn built_in_resolves_all_ten_themes_distinctly() -> Result<(), String> {
        let dark = ExportTheme::built_in(Some("dark"));
        let m3_light = ExportTheme::built_in(Some("m3-light"));
        assert_eq!(css_var(&dark, "accent").as_deref(), Some("#52a8ff"));
        assert_eq!(css_var(&m3_light, "accent").as_deref(), Some("#6750a4"));
        assert_ne!(
            css_var(&dark, "accent"),
            css_var(&m3_light, "accent"),
            "m3-light must differ from dark"
        );

        let mut accents = std::collections::BTreeSet::new();
        for name in BUILT_IN_THEME_NAMES {
            let theme = ExportTheme::built_in(Some(name));
            let accent = css_var(&theme, "accent").ok_or_else(|| format!("{name}: accent slot"))?;
            accents.insert((name, accent));
        }
        assert_eq!(accents.len(), 10, "each built-in should resolve distinctly");

        let unknown = ExportTheme::built_in(Some("not-a-theme"));
        assert_eq!(css_var(&unknown, "accent").as_deref(), Some("#52a8ff"));
        assert_eq!(unknown.page_background.as_deref(), Some("#18181e"));
        Ok(())
    }

    #[test]
    fn resolve_export_theme_honors_m3_light_and_auto_dark() -> Result<(), Box<dyn std::error::Error>>
    {
        let m3 = resolve_export_theme(Some("m3-light"), ThemeMode::Light);
        assert_eq!(css_var(&m3, "accent").as_deref(), Some("#6750a4"));
        assert_eq!(css_var(&m3, "text").as_deref(), Some("#1d1b20"));

        let auto_dark = resolve_export_theme(Some("dark"), ThemeMode::Auto);
        assert_eq!(css_var(&auto_dark, "accent").as_deref(), Some("#52a8ff"));
        assert_eq!(css_var(&auto_dark, "text").as_deref(), Some("#ededed"));
        assert_eq!(auto_dark.page_background.as_deref(), Some("#18181e"));

        let root = tempdir()?;
        let input = fixture(root.path())?;
        let output = root.path().join("m3.html");
        export_from_file(
            &input.to_string_lossy(),
            ExportOptions {
                output_path: Some(output.clone()),
                theme: Some(resolve_export_theme(Some("m3-light"), ThemeMode::Light)),
                ..ExportOptions::default()
            },
        )?;
        let html = fs::read_to_string(output)?;
        assert!(
            html.contains("--accent: #6750a4;"),
            "m3-light CSS variables should be embedded"
        );
        assert!(
            html.contains("--text: #1d1b20;"),
            "m3-light text slot should be embedded"
        );

        let output = root.path().join("auto-dark.html");
        export_from_file(
            &input.to_string_lossy(),
            ExportOptions {
                output_path: Some(output.clone()),
                theme: Some(resolve_export_theme(Some("dark"), ThemeMode::Auto)),
                ..ExportOptions::default()
            },
        )?;
        let html = fs::read_to_string(output)?;
        assert!(
            html.contains("--accent: #52a8ff;"),
            "auto+dark headless should export default dark"
        );
        assert!(html.contains("--exportPageBg: #18181e;"));
        Ok(())
    }
}
