//! Markdown YAML frontmatter extraction and parsing.
//!
//! Port of `.references/pi/packages/coding-agent/src/utils/frontmatter.ts`.
//! Uses `serde-saphyr` for real YAML parsing of the frontmatter block.

use std::collections::BTreeMap;

use serde::Deserialize;
use serde_json::Value;

/// Parsed frontmatter plus the remaining markdown body.
#[derive(Clone, Debug, PartialEq)]
pub struct ParsedFrontmatter {
    /// YAML mapping (empty when no frontmatter block is present).
    pub frontmatter: BTreeMap<String, Value>,
    /// Body after the closing `---` (trimmed when a block was found).
    pub body: String,
}

/// Error while parsing frontmatter YAML.
#[derive(Debug, thiserror::Error)]
pub enum FrontmatterError {
    /// YAML document could not be deserialized.
    #[error("{0}")]
    Yaml(String),
}

/// Normalize CRLF / bare CR to LF (TypeScript `normalizeNewlines`).
#[must_use]
pub fn normalize_newlines(value: &str) -> String {
    value.replace("\r\n", "\n").replace('\r', "\n")
}

/// Extract the YAML string and body from markdown content.
///
/// Rules match TypeScript exactly:
/// - content must start with `---`
/// - closing fence is the first `"\n---"` after the opener
/// - YAML is `slice(4, endIndex)`
/// - body is `slice(endIndex + 4).trim()`
#[must_use]
pub fn extract_frontmatter(content: &str) -> (Option<String>, String) {
    let normalized = normalize_newlines(content);
    if !normalized.starts_with("---") {
        return (None, normalized);
    }
    let Some(end_index) = normalized[3..].find("\n---").map(|i| i + 3) else {
        return (None, normalized);
    };
    let yaml_string = normalized[4..end_index].to_owned();
    let body = normalized[end_index + 4..].trim().to_owned();
    (Some(yaml_string), body)
}

/// Parse markdown frontmatter into a string→JSON map and body.
///
/// # Errors
///
/// Returns [`FrontmatterError::Yaml`] when the frontmatter block is not valid YAML.
pub fn parse_frontmatter(content: &str) -> Result<ParsedFrontmatter, FrontmatterError> {
    let (yaml_string, body) = extract_frontmatter(content);
    let Some(yaml_string) = yaml_string else {
        return Ok(ParsedFrontmatter {
            frontmatter: BTreeMap::new(),
            body,
        });
    };
    if yaml_string.trim().is_empty() {
        return Ok(ParsedFrontmatter {
            frontmatter: BTreeMap::new(),
            body,
        });
    }
    let value: Value = serde_saphyr::from_str(&yaml_string)
        .map_err(|error| FrontmatterError::Yaml(error.to_string()))?;
    let frontmatter = match value {
        Value::Null => BTreeMap::new(),
        Value::Object(map) => map.into_iter().collect(),
        other => {
            // Non-mapping documents are treated as empty maps for skill/prompt
            // frontmatter (TS casts `parsed ?? {}`).
            let _ = other;
            BTreeMap::new()
        }
    };
    Ok(ParsedFrontmatter { frontmatter, body })
}

/// Body only after stripping frontmatter.
///
/// # Errors
///
/// Returns [`FrontmatterError::Yaml`] when the frontmatter block is not valid YAML.
pub fn strip_frontmatter(content: &str) -> Result<String, FrontmatterError> {
    Ok(parse_frontmatter(content)?.body)
}

/// Read a string field from parsed frontmatter.
#[must_use]
pub fn frontmatter_string(map: &BTreeMap<String, Value>, key: &str) -> Option<String> {
    match map.get(key) {
        Some(Value::String(s)) => Some(s.clone()),
        Some(Value::Bool(b)) => Some(b.to_string()),
        Some(Value::Number(n)) => Some(n.to_string()),
        Some(other) => other.as_str().map(str::to_owned),
        None => None,
    }
}

/// Read a boolean field from parsed frontmatter.
#[must_use]
pub fn frontmatter_bool(map: &BTreeMap<String, Value>, key: &str) -> Option<bool> {
    match map.get(key) {
        Some(Value::Bool(b)) => Some(*b),
        Some(Value::String(s)) if s == "true" => Some(true),
        Some(Value::String(s)) if s == "false" => Some(false),
        _ => None,
    }
}

/// Deserialize a typed frontmatter document from a YAML string.
///
/// # Errors
///
/// Returns [`FrontmatterError::Yaml`] on parse failure.
pub fn parse_yaml_value<T>(yaml: &str) -> Result<T, FrontmatterError>
where
    T: for<'de> Deserialize<'de>,
{
    serde_saphyr::from_str(yaml).map_err(|error| FrontmatterError::Yaml(error.to_string()))
}

#[cfg(test)]
mod tests {
    use super::*;

    type TestResult = Result<(), Box<dyn std::error::Error>>;

    #[test]
    fn normalize_crlf_and_cr() {
        assert_eq!(normalize_newlines("a\r\nb\rc"), "a\nb\nc");
    }

    #[test]
    fn no_frontmatter_returns_full_body() -> TestResult {
        let parsed = parse_frontmatter("hello\nworld")?;
        assert!(parsed.frontmatter.is_empty());
        assert_eq!(parsed.body, "hello\nworld");
        Ok(())
    }

    #[test]
    fn extracts_yaml_and_trims_body() -> TestResult {
        let content = "---\nname: foo\ndescription: bar\n---\n\n# Title\n";
        let parsed = parse_frontmatter(content)?;
        assert_eq!(
            frontmatter_string(&parsed.frontmatter, "name").as_deref(),
            Some("foo")
        );
        assert_eq!(
            frontmatter_string(&parsed.frontmatter, "description").as_deref(),
            Some("bar")
        );
        assert_eq!(parsed.body, "# Title");
        Ok(())
    }

    #[test]
    fn supports_comments_and_bools() -> TestResult {
        let content = "---\n# comment\nname: x\ndisable-model-invocation: true\n---\nbody";
        let parsed = parse_frontmatter(content)?;
        assert_eq!(
            frontmatter_bool(&parsed.frontmatter, "disable-model-invocation"),
            Some(true)
        );
        assert_eq!(parsed.body, "body");
        Ok(())
    }

    #[test]
    fn yaml_error_surfaces() -> TestResult {
        let content = "---\n: : bad\n---\nbody";
        let Err(err) = parse_frontmatter(content) else {
            return Err("expected yaml parse failure".into());
        };
        assert!(!err.to_string().is_empty());
        Ok(())
    }

    #[test]
    fn unclosed_fence_is_not_frontmatter() -> TestResult {
        let content = "---\nname: x\nbody without close";
        let parsed = parse_frontmatter(content)?;
        assert!(parsed.frontmatter.is_empty());
        assert_eq!(parsed.body, content.replace('\r', ""));
        Ok(())
    }

    #[test]
    fn strip_frontmatter_returns_body() -> TestResult {
        let body = strip_frontmatter("---\na: 1\n---\nhello")?;
        assert_eq!(body, "hello");
        Ok(())
    }
}
