//! Constrained-sampling algorithms for tool-level JSON-schema and grammar
//! constrained sampling.
//!
//! Port of `api/constrained-sampling.ts` from the reference TypeScript
//! implementation.

use std::collections::{BTreeMap, HashSet};

use serde::Serialize;
use serde_json::{Map, Value};

use crate::types::{
    ConstrainedSampling, ConstrainedSamplingConfig, GrammarVariants, StrictMode, Tool,
};

// ---------------------------------------------------------------------------
// Errors
// ---------------------------------------------------------------------------

/// A tool schema that cannot be expressed in the provider strict subset.
#[derive(Clone, Debug, thiserror::Error)]
#[error("{0}")]
pub struct UnsupportedStrictJsonSchema(String);

/// A constrained-sampling request that cannot be satisfied.
#[derive(Clone, Debug, thiserror::Error)]
#[error("{0}")]
pub struct ConstrainedSamplingError(String);

// ---------------------------------------------------------------------------
// Strict JSON-schema transform
// ---------------------------------------------------------------------------

const UNSUPPORTED_STRICT_KEYS: &[&str] = &[
    "$ref",
    "$defs",
    "definitions",
    "allOf",
    "oneOf",
    "patternProperties",
    "dependentSchemas",
    "dependencies",
    "unevaluatedProperties",
    "propertyNames",
    "contains",
    "prefixItems",
    "not",
    "if",
    "then",
    "else",
];

/// Mirrors `isStructuredSchema`: an object/array `type`, or `properties`/`items` present.
fn is_structured_schema(schema: &Value) -> bool {
    let Some(map) = schema.as_object() else {
        return false;
    };
    let types: Vec<&str> = match map.get("type") {
        Some(Value::String(single)) => vec![single.as_str()],
        Some(Value::Array(items)) => items.iter().filter_map(Value::as_str).collect(),
        _ => Vec::new(),
    };
    types.contains(&"object")
        || types.contains(&"array")
        || map.contains_key("properties")
        || map.contains_key("items")
}

/// Mirrors `schemaAllowsNull`.
fn schema_allows_null(schema: &Value) -> bool {
    let Some(map) = schema.as_object() else {
        return false;
    };
    match map.get("type") {
        Some(Value::String(single)) if single == "null" => return true,
        Some(Value::Array(items)) if items.iter().any(|item| item.as_str() == Some("null")) => {
            return true;
        }
        _ => {}
    }
    if map.get("const") == Some(&Value::Null) {
        return true;
    }
    if let Some(Value::Array(items)) = map.get("enum")
        && items.iter().any(Value::is_null)
    {
        return true;
    }
    matches!(
        map.get("anyOf"),
        Some(Value::Array(variants)) if variants.iter().any(schema_allows_null)
    )
}

/// Depth-first transform of a JSON-schema node to the strict subset expected
/// by provider constrained sampling. Mirrors `makeJsonSchemaNodeStrict`,
/// mutating `schema` in place.
fn make_json_schema_node_strict(schema: &mut Value) -> Result<(), UnsupportedStrictJsonSchema> {
    let Value::Object(map) = schema else {
        return Err(UnsupportedStrictJsonSchema(
            "boolean schemas are unsupported".into(),
        ));
    };

    for key in UNSUPPORTED_STRICT_KEYS {
        if map.contains_key(*key) {
            return Err(UnsupportedStrictJsonSchema(format!(
                "{key} schemas are unsupported"
            )));
        }
    }

    validate_any_of_variants(map)?;
    validate_items_schema(map)?;

    let is_object_schema = map.get("type") == Some(&Value::String("object".to_owned()));
    if map.contains_key("properties") && !is_object_schema {
        return Err(UnsupportedStrictJsonSchema(
            "properties require type object".into(),
        ));
    }
    if !is_object_schema {
        return Ok(());
    }

    validate_object_schema(map)?;
    let property_names: Vec<String> = map
        .get("properties")
        .and_then(Value::as_object)
        .map(|properties| properties.keys().cloned().collect())
        .unwrap_or_default();
    let original_required: Vec<String> = map
        .get("required")
        .and_then(Value::as_array)
        .map(|items| {
            items
                .iter()
                .filter_map(|item| item.as_str().map(str::to_owned))
                .collect()
        })
        .unwrap_or_default();
    if original_required
        .iter()
        .any(|name| !property_names.contains(name))
    {
        return Err(UnsupportedStrictJsonSchema(
            "required contains an unknown property".into(),
        ));
    }
    let required: HashSet<String> = original_required.iter().cloned().collect();
    make_properties_strict(map, &required)?;

    let mut final_required = original_required;
    for name in &property_names {
        if !final_required.contains(name) {
            final_required.push(name.clone());
        }
    }
    map.insert(
        "required".to_owned(),
        Value::Array(final_required.into_iter().map(Value::String).collect()),
    );
    map.insert("additionalProperties".to_owned(), Value::Bool(false));

    Ok(())
}

fn validate_any_of_variants(
    map: &mut Map<String, Value>,
) -> Result<(), UnsupportedStrictJsonSchema> {
    let Some(any_of) = map.get_mut("anyOf") else {
        return Ok(());
    };
    let Value::Array(variants) = any_of else {
        return Err(UnsupportedStrictJsonSchema(
            "anyOf must contain at least one schema".into(),
        ));
    };
    if variants.is_empty() {
        return Err(UnsupportedStrictJsonSchema(
            "anyOf must contain at least one schema".into(),
        ));
    }
    for variant in variants {
        if is_structured_schema(variant) {
            return Err(UnsupportedStrictJsonSchema(
                "object and array unions are unsupported".into(),
            ));
        }
        make_json_schema_node_strict(variant)?;
    }
    Ok(())
}

fn validate_items_schema(map: &mut Map<String, Value>) -> Result<(), UnsupportedStrictJsonSchema> {
    let Some(items) = map.get_mut("items") else {
        return Ok(());
    };
    if items.is_array() {
        return Err(UnsupportedStrictJsonSchema(
            "tuple schemas are unsupported".into(),
        ));
    }
    make_json_schema_node_strict(items)
}

fn validate_object_schema(map: &Map<String, Value>) -> Result<(), UnsupportedStrictJsonSchema> {
    if let Some(additional) = map.get("additionalProperties")
        && additional != &Value::Bool(false)
    {
        return Err(UnsupportedStrictJsonSchema(
            "schema-valued or true additionalProperties is unsupported".into(),
        ));
    }
    if let Some(properties) = map.get("properties")
        && !properties.is_object()
    {
        return Err(UnsupportedStrictJsonSchema(
            "object properties must be a schema map".into(),
        ));
    }
    if let Some(required) = map.get("required") {
        let is_valid_string_array =
            matches!(required, Value::Array(items) if items.iter().all(Value::is_string));
        if !is_valid_string_array {
            return Err(UnsupportedStrictJsonSchema(
                "object required must be a string array".into(),
            ));
        }
    }
    Ok(())
}

fn make_properties_strict(
    map: &mut Map<String, Value>,
    required: &HashSet<String>,
) -> Result<(), UnsupportedStrictJsonSchema> {
    let Some(properties) = map.get_mut("properties").and_then(Value::as_object_mut) else {
        return Ok(());
    };
    for (name, property) in properties {
        make_json_schema_node_strict(property)?;
        if !required.contains(name) && !schema_allows_null(property) {
            *property = Value::Object(Map::from_iter([(
                "anyOf".to_owned(),
                Value::Array(vec![property.clone(), serde_json::json!({"type": "null"})]),
            )]));
        }
    }
    Ok(())
}

/// Convert a tool schema to the strict subset expected by provider constrained
/// sampling.
///
/// # Errors
/// Returns [`UnsupportedStrictJsonSchema`] when the schema uses a construct the
/// strict subset forbids: `$ref`, `$defs`, `definitions`, `allOf`, `oneOf`,
/// `patternProperties`, `dependentSchemas`, `dependencies`,
/// `unevaluatedProperties`, `propertyNames`, `contains`, `prefixItems`, `not`,
/// `if`, `then`, `else`, tuple `items`, structured `anyOf` variants,
/// schema-valued or `true` `additionalProperties`, or a non-object root.
pub fn make_strict_json_schema(schema: &Value) -> Result<Value, UnsupportedStrictJsonSchema> {
    let mut cloned = schema.clone();
    if !cloned.is_object() {
        return Err(UnsupportedStrictJsonSchema(
            "root schema must have type object".into(),
        ));
    }
    make_json_schema_node_strict(&mut cloned)?;
    if cloned.get("type") != Some(&Value::String("object".to_owned())) {
        return Err(UnsupportedStrictJsonSchema(
            "root schema must have type object".into(),
        ));
    }
    Ok(cloned)
}

// ---------------------------------------------------------------------------
// JSON-schema resolver
// ---------------------------------------------------------------------------

/// Resolve JSON-schema constrained sampling for one tool.
///
/// `Ok(Some(parameters))` means strict applies and `parameters` is the
/// transformed schema. `Ok(None)` means strict does not apply: no config, a
/// grammar config, a provider that cannot do it under [`StrictMode::Prefer`],
/// or a schema outside the strict subset under [`StrictMode::Prefer`]. That
/// mirrors TypeScript `undefined`.
///
/// # Errors
/// Returns [`ConstrainedSamplingError`] when the tool sets
/// [`StrictMode::Require`] and strict sampling is unavailable.
pub fn resolve_json_schema_strict_sampling(
    tool: &Tool,
    supports_strict_mode: bool,
) -> Result<Option<Value>, ConstrainedSamplingError> {
    let strict = match &tool.constrained_sampling {
        Some(ConstrainedSampling::Config(ConstrainedSamplingConfig::JsonSchema { strict })) => {
            *strict
        }
        _ => return Ok(None),
    };

    if supports_strict_mode {
        return match make_strict_json_schema(&tool.parameters) {
            Ok(schema) => Ok(Some(schema)),
            Err(error) if strict == StrictMode::Require => Err(ConstrainedSamplingError(format!(
                "Tool \"{}\" requires JSON-schema constrained sampling, but {error}.",
                tool.name
            ))),
            Err(_) => Ok(None),
        };
    }

    if strict == StrictMode::Require {
        return Err(ConstrainedSamplingError(format!(
            "Tool \"{}\" requires JSON-schema constrained sampling, but strict tools are unsupported.",
            tool.name
        )));
    }

    Ok(None)
}

// ---------------------------------------------------------------------------
// Grammar constrained sampling
// ---------------------------------------------------------------------------

/// Grammar dialect written into the provider payload.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum GrammarSyntax {
    /// Lark grammar, from the `openai_lark` variant.
    Lark,
    /// Regular expression, from the `openai_regex` variant.
    Regex,
}

/// Grammar constrained sampling resolved for one tool.
#[derive(Clone, Debug, PartialEq)]
pub struct GrammarConstrainedSampling {
    /// Dialect written as the payload `syntax`.
    pub syntax: GrammarSyntax,
    /// Grammar definition text.
    pub definition: String,
    /// The single required string property carrying raw model output.
    pub input_property: String,
}

/// Mirrors `inferGrammarInputProperty`.
fn infer_grammar_input_property(tool: &Tool) -> Result<String, ConstrainedSamplingError> {
    if tool.parameters.get("type") != Some(&Value::String("object".to_owned())) {
        return Err(ConstrainedSamplingError(
            "grammar constrained sampling requires an object parameter schema".into(),
        ));
    }

    let input_property = tool
        .parameters
        .get("required")
        .and_then(Value::as_array)
        .filter(|required| required.len() == 1)
        .and_then(|required| required[0].as_str());
    let Some(input_property) = input_property else {
        return Err(ConstrainedSamplingError(
            "grammar constrained sampling requires exactly one required string property".into(),
        ));
    };

    let property_schema = tool
        .parameters
        .get("properties")
        .and_then(|properties| properties.get(input_property));
    let Some(property_schema) = property_schema else {
        return Err(ConstrainedSamplingError(format!(
            "grammar constrained sampling requires a properties entry for {input_property}"
        )));
    };
    if property_schema.get("type") != Some(&Value::String("string".to_owned())) {
        return Err(ConstrainedSamplingError(format!(
            "grammar constrained sampling property {input_property} must have type string"
        )));
    }

    Ok(input_property.to_owned())
}

/// Resolve grammar constrained sampling for one tool.
///
/// # Errors
/// Returns [`ConstrainedSamplingError`] when a grammar config supplies no
/// non-blank supported variant, or when the parameter schema is not an object
/// with exactly one required string property that has a `properties` entry.
pub fn resolve_grammar_constrained_sampling(
    tool: &Tool,
    supports_openai_grammar_tools: bool,
) -> Result<Option<GrammarConstrainedSampling>, ConstrainedSamplingError> {
    let variants: &GrammarVariants = match &tool.constrained_sampling {
        Some(ConstrainedSampling::Config(ConstrainedSamplingConfig::Grammar { variants })) => {
            variants
        }
        _ => return Ok(None),
    };

    if !supports_openai_grammar_tools {
        return Ok(None);
    }

    let has_lark = variants
        .openai_lark
        .as_deref()
        .is_some_and(|value| !value.trim().is_empty());
    let has_regex = variants
        .openai_regex
        .as_deref()
        .is_some_and(|value| !value.trim().is_empty());
    if !has_lark && !has_regex {
        return Err(ConstrainedSamplingError(format!(
            "Tool \"{}\" cannot use grammar constrained sampling: no supported grammar variant was provided.",
            tool.name
        )));
    }

    let input_property = infer_grammar_input_property(tool).map_err(|error| {
        ConstrainedSamplingError(format!(
            "Tool \"{}\" cannot use grammar constrained sampling: {error}.",
            tool.name
        ))
    })?;

    let (syntax, definition) = if has_lark {
        (
            GrammarSyntax::Lark,
            variants.openai_lark.clone().unwrap_or_default(),
        )
    } else {
        (
            GrammarSyntax::Regex,
            variants.openai_regex.clone().unwrap_or_default(),
        )
    };

    Ok(Some(GrammarConstrainedSampling {
        syntax,
        definition,
        input_property,
    }))
}

/// Map each grammar tool's name to its input property.
///
/// # Errors
/// Propagates [`resolve_grammar_constrained_sampling`] failures.
pub fn grammar_tool_input_properties(
    tools: Option<&[Tool]>,
    supports_openai_grammar_tools: bool,
) -> Result<BTreeMap<String, String>, ConstrainedSamplingError> {
    let mut properties = BTreeMap::new();
    for tool in tools.unwrap_or_default() {
        if let Some(grammar) =
            resolve_grammar_constrained_sampling(tool, supports_openai_grammar_tools)?
        {
            properties.insert(tool.name.clone(), grammar.input_property);
        }
    }
    Ok(properties)
}

/// Read a grammar tool call's raw input out of its decoded arguments.
///
/// # Errors
/// Returns [`ConstrainedSamplingError`] when the argument is missing or is not a string.
pub fn grammar_tool_input<'a>(
    tool_name: &str,
    arguments: &'a Map<String, Value>,
    input_property: &str,
) -> Result<&'a str, ConstrainedSamplingError> {
    arguments.get(input_property).and_then(Value::as_str).ok_or_else(|| {
        ConstrainedSamplingError(format!(
            "Grammar tool call \"{tool_name}\" requires argument \"{input_property}\" to be a string."
        ))
    })
}

// ---------------------------------------------------------------------------
// Grammar streaming buffer
// ---------------------------------------------------------------------------

/// Incremental JSON encoder for a grammar tool call's single string argument.
#[derive(Clone, Debug, Default)]
pub struct GrammarToolInputBuffer {
    input: String,
    started: bool,
    closed: bool,
}

impl GrammarToolInputBuffer {
    /// Fold the cumulative `next_input` into the buffer and return the JSON
    /// fragment to publish as a `toolcall_delta`, or `None` when there is
    /// nothing to publish.
    ///
    /// # Errors
    /// Returns [`ConstrainedSamplingError`] when `next_input` is not a prefix
    /// extension of what was buffered, or when it changes after close.
    pub fn append(
        &mut self,
        input_property: &str,
        next_input: &str,
        close: bool,
    ) -> Result<Option<String>, ConstrainedSamplingError> {
        if self.closed {
            if close && next_input == self.input {
                return Ok(None);
            }
            return Err(ConstrainedSamplingError(format!(
                "grammar tool input for property \"{input_property}\" changed after it was closed"
            )));
        }
        if !next_input.starts_with(&self.input) {
            return Err(ConstrainedSamplingError(format!(
                "grammar tool input for property \"{input_property}\" changed non-monotonically"
            )));
        }

        let delta = &next_input[self.input.len()..];
        if !close && delta.is_empty() {
            return Ok(None);
        }

        let mut fragment = String::new();
        if !self.started {
            let key_json = serde_json::to_string(input_property).map_err(|error| {
                ConstrainedSamplingError(format!(
                    "failed to encode grammar input property: {error}"
                ))
            })?;
            fragment.push('{');
            fragment.push_str(&key_json);
            fragment.push_str(":\"");
            self.started = true;
        }
        let escaped = serde_json::to_string(delta).map_err(|error| {
            ConstrainedSamplingError(format!("failed to encode grammar input delta: {error}"))
        })?;
        let escaped = escaped
            .strip_prefix('"')
            .and_then(|value| value.strip_suffix('"'))
            .ok_or_else(|| {
                ConstrainedSamplingError(
                    "JSON string serialization did not produce a quoted string".into(),
                )
            })?;
        fragment.push_str(escaped);
        next_input.clone_into(&mut self.input);

        if close {
            fragment.push_str("\"}");
            self.closed = true;
        }

        Ok(Some(fragment))
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
#[expect(
    clippy::unwrap_used,
    reason = "test fixtures intentionally assert valid and invalid fallible results"
)]
mod tests {
    use serde_json::json;

    use super::*;

    fn tool(name: &str, parameters: Value) -> Tool {
        Tool {
            name: name.to_owned(),
            description: "d".to_owned(),
            parameters,
            constrained_sampling: None,
        }
    }

    fn json_schema_tool(name: &str, parameters: Value, strict: StrictMode) -> Tool {
        let mut t = tool(name, parameters);
        t.constrained_sampling = Some(ConstrainedSampling::Config(
            ConstrainedSamplingConfig::JsonSchema { strict },
        ));
        t
    }

    fn grammar_tool(name: &str, variants: GrammarVariants) -> Tool {
        let mut t = tool(
            name,
            json!({
                "type": "object",
                "properties": { "code": { "type": "string" } },
                "required": ["code"]
            }),
        );
        t.constrained_sampling = Some(ConstrainedSampling::Config(
            ConstrainedSamplingConfig::Grammar { variants },
        ));
        t
    }

    // ----- make_strict_json_schema: rejections -----

    #[test]
    fn strict_rejects_boolean_schema_at_root() {
        let error = make_strict_json_schema(&json!(true)).unwrap_err();
        assert_eq!(error.to_string(), "root schema must have type object");
        let error = make_strict_json_schema(&json!(false)).unwrap_err();
        assert_eq!(error.to_string(), "root schema must have type object");
    }

    #[test]
    fn strict_rejects_boolean_schema_nested_in_properties() {
        let error = make_strict_json_schema(&json!({"type": "object", "properties": {"x": true}}))
            .unwrap_err();
        assert_eq!(error.to_string(), "boolean schemas are unsupported");
    }

    #[test]
    fn strict_rejects_ref() {
        let error =
            make_strict_json_schema(&json!({"$ref": "#/$defs/Foo", "type": "object"})).unwrap_err();
        assert_eq!(error.to_string(), "$ref schemas are unsupported");
    }

    #[test]
    fn strict_rejects_allof() {
        let error =
            make_strict_json_schema(&json!({"allOf": [{"type": "string"}], "type": "object"}))
                .unwrap_err();
        assert_eq!(error.to_string(), "allOf schemas are unsupported");
    }

    #[test]
    fn strict_rejects_empty_any_of() {
        let error = make_strict_json_schema(&json!({"type": "object", "anyOf": []})).unwrap_err();
        assert_eq!(error.to_string(), "anyOf must contain at least one schema");
    }

    #[test]
    fn strict_rejects_structured_any_of_variant() {
        let error = make_strict_json_schema(&json!({
            "type": "object",
            "anyOf": [{"type": "object", "properties": {}}]
        }))
        .unwrap_err();
        assert_eq!(error.to_string(), "object and array unions are unsupported");
    }

    #[test]
    fn strict_allows_primitive_any_of_variants() {
        let transformed = make_strict_json_schema(&json!({
            "type": "object",
            "anyOf": [{"type": "string"}, {"type": "number"}]
        }))
        .unwrap();
        assert_eq!(
            transformed["anyOf"],
            json!([{"type": "string"}, {"type": "number"}])
        );
    }

    #[test]
    fn strict_rejects_tuple_items() {
        let error =
            make_strict_json_schema(&json!({"type": "object", "items": ["a", "b"]})).unwrap_err();
        assert_eq!(error.to_string(), "tuple schemas are unsupported");
    }

    #[test]
    fn strict_rejects_properties_without_type_object() {
        let error =
            make_strict_json_schema(&json!({"properties": {"x": {"type": "string"}}})).unwrap_err();
        assert_eq!(error.to_string(), "properties require type object");
    }

    #[test]
    fn strict_rejects_non_object_root_type() {
        let error = make_strict_json_schema(&json!({"type": "string"})).unwrap_err();
        assert_eq!(error.to_string(), "root schema must have type object");
    }

    #[test]
    fn strict_passes_through_non_object_non_property_schema() {
        // A plain array-typed schema has no `properties`, so it is returned
        // unchanged past the object-only checks, then rejected only for the
        // final root-must-be-object requirement.
        let error = make_strict_json_schema(&json!({"type": "array", "items": {"type": "string"}}))
            .unwrap_err();
        assert_eq!(error.to_string(), "root schema must have type object");
    }

    #[test]
    fn strict_rejects_schema_valued_additional_properties() {
        let error = make_strict_json_schema(&json!({
            "type": "object",
            "additionalProperties": {"type": "string"}
        }))
        .unwrap_err();
        assert_eq!(
            error.to_string(),
            "schema-valued or true additionalProperties is unsupported"
        );
    }

    #[test]
    fn strict_rejects_true_additional_properties() {
        let error =
            make_strict_json_schema(&json!({"type": "object", "additionalProperties": true}))
                .unwrap_err();
        assert_eq!(
            error.to_string(),
            "schema-valued or true additionalProperties is unsupported"
        );
    }

    #[test]
    fn strict_rejects_non_object_properties_map() {
        let error =
            make_strict_json_schema(&json!({"type": "object", "properties": []})).unwrap_err();
        assert_eq!(error.to_string(), "object properties must be a schema map");
    }

    #[test]
    fn strict_rejects_non_string_array_required() {
        let error =
            make_strict_json_schema(&json!({"type": "object", "required": "path"})).unwrap_err();
        assert_eq!(error.to_string(), "object required must be a string array");
        let error = make_strict_json_schema(&json!({"type": "object", "required": ["path", 1]}))
            .unwrap_err();
        assert_eq!(error.to_string(), "object required must be a string array");
    }

    #[test]
    fn strict_rejects_required_naming_unknown_property() {
        let error = make_strict_json_schema(&json!({
            "type": "object",
            "properties": {"a": {"type": "string"}},
            "required": ["b"]
        }))
        .unwrap_err();
        assert_eq!(error.to_string(), "required contains an unknown property");
    }

    // ----- make_strict_json_schema: V1/V2 boundary transforms -----

    #[test]
    fn strict_v1_sets_required_and_additional_properties_false() {
        let transformed = make_strict_json_schema(&json!({
            "type": "object",
            "properties": {"path": {"type": "string"}},
            "required": ["path"]
        }))
        .unwrap();
        assert_eq!(transformed["required"], json!(["path"]));
        assert_eq!(transformed["additionalProperties"], json!(false));
        assert_eq!(transformed["properties"]["path"], json!({"type": "string"}));
    }

    #[test]
    fn strict_v2_wraps_optional_property_and_extends_required() {
        let transformed = make_strict_json_schema(&json!({
            "type": "object",
            "properties": {
                "path": {"type": "string"},
                "offset": {"type": "number"}
            },
            "required": ["path"]
        }))
        .unwrap();
        assert_eq!(
            transformed["properties"]["offset"],
            json!({"anyOf": [{"type": "number"}, {"type": "null"}]})
        );
        assert_eq!(transformed["required"], json!(["path", "offset"]));
        assert_eq!(transformed["properties"]["path"], json!({"type": "string"}));
    }

    #[test]
    fn strict_leaves_already_nullable_optional_property_unwrapped() {
        let transformed = make_strict_json_schema(&json!({
            "type": "object",
            "properties": {
                "path": {"type": "string"},
                "offset": {"type": ["number", "null"]}
            },
            "required": ["path"]
        }))
        .unwrap();
        assert_eq!(
            transformed["properties"]["offset"],
            json!({"type": ["number", "null"]})
        );
    }

    #[test]
    fn strict_recurses_into_nested_object_properties() {
        let transformed = make_strict_json_schema(&json!({
            "type": "object",
            "properties": {
                "inner": {
                    "type": "object",
                    "properties": {"a": {"type": "string"}},
                    "required": ["a"]
                }
            },
            "required": ["inner"]
        }))
        .unwrap();
        let inner = &transformed["properties"]["inner"];
        assert_eq!(inner["additionalProperties"], json!(false));
        assert_eq!(inner["required"], json!(["a"]));
    }

    #[test]
    fn strict_handles_object_with_no_properties() {
        let transformed = make_strict_json_schema(&json!({"type": "object"})).unwrap();
        assert_eq!(transformed["required"], json!([]));
        assert_eq!(transformed["additionalProperties"], json!(false));
    }

    // ----- resolve_json_schema_strict_sampling: fallback/error semantics -----

    #[test]
    fn resolve_json_schema_returns_none_when_config_absent() {
        let t = tool("x", json!({"type": "object"}));
        assert_eq!(resolve_json_schema_strict_sampling(&t, true).unwrap(), None);
    }

    #[test]
    fn resolve_json_schema_returns_none_when_disabled() {
        let mut t = tool("x", json!({"type": "object"}));
        t.constrained_sampling = Some(ConstrainedSampling::Disabled);
        assert_eq!(resolve_json_schema_strict_sampling(&t, true).unwrap(), None);
    }

    #[test]
    fn resolve_json_schema_returns_none_for_grammar_config() {
        let t = grammar_tool("x", GrammarVariants::default());
        assert_eq!(resolve_json_schema_strict_sampling(&t, true).unwrap(), None);
    }

    #[test]
    fn resolve_json_schema_v3_prefer_falls_back_on_unsupported_schema_without_error() {
        let schema = json!({"$ref": "#/$defs/x"});
        let t = json_schema_tool("x", schema.clone(), StrictMode::Prefer);
        let result = resolve_json_schema_strict_sampling(&t, true).unwrap();
        assert_eq!(result, None);
        assert_eq!(
            t.parameters, schema,
            "schema must remain verbatim on fallback"
        );
    }

    #[test]
    fn resolve_json_schema_prefer_returns_transformed_schema_when_supported() {
        let t = json_schema_tool(
            "x",
            json!({"type": "object", "properties": {"path": {"type": "string"}}, "required": ["path"]}),
            StrictMode::Prefer,
        );
        let result = resolve_json_schema_strict_sampling(&t, true)
            .unwrap()
            .unwrap();
        assert_eq!(result["additionalProperties"], json!(false));
    }

    #[test]
    fn resolve_json_schema_prefer_without_provider_support_returns_none() {
        let t = json_schema_tool("x", json!({"type": "object"}), StrictMode::Prefer);
        assert_eq!(
            resolve_json_schema_strict_sampling(&t, false).unwrap(),
            None
        );
    }

    #[test]
    fn resolve_json_schema_v4_require_with_unsupported_schema_reports_exact_message() {
        let t = json_schema_tool("x", json!({"$ref": "#/$defs/x"}), StrictMode::Require);
        let error = resolve_json_schema_strict_sampling(&t, true).unwrap_err();
        assert_eq!(
            error.to_string(),
            "Tool \"x\" requires JSON-schema constrained sampling, but $ref schemas are unsupported."
        );
    }

    #[test]
    fn resolve_json_schema_v5_require_without_strict_mode_reports_exact_message() {
        let t = json_schema_tool("x", json!({"type": "object"}), StrictMode::Require);
        let error = resolve_json_schema_strict_sampling(&t, false).unwrap_err();
        assert_eq!(
            error.to_string(),
            "Tool \"x\" requires JSON-schema constrained sampling, but strict tools are unsupported."
        );
    }

    // ----- resolve_grammar_constrained_sampling -----

    #[test]
    fn resolve_grammar_returns_none_when_config_absent() {
        let t = tool("x", json!({"type": "object"}));
        assert_eq!(
            resolve_grammar_constrained_sampling(&t, true).unwrap(),
            None
        );
    }

    #[test]
    fn resolve_grammar_returns_none_when_disabled() {
        let mut t = grammar_tool(
            "x",
            GrammarVariants {
                openai_lark: Some("start: /[a-z]+/".into()),
                openai_regex: None,
            },
        );
        t.constrained_sampling = Some(ConstrainedSampling::Disabled);
        assert_eq!(
            resolve_grammar_constrained_sampling(&t, true).unwrap(),
            None
        );
    }

    #[test]
    fn resolve_grammar_returns_none_for_json_schema_config() {
        let t = json_schema_tool("x", json!({"type": "object"}), StrictMode::Prefer);
        assert_eq!(
            resolve_grammar_constrained_sampling(&t, true).unwrap(),
            None
        );
    }

    #[test]
    fn resolve_grammar_v6_lark_variant_wins_over_regex() {
        let t = grammar_tool(
            "x",
            GrammarVariants {
                openai_lark: Some("start: /[a-z]+/".into()),
                openai_regex: Some("^[a-z]+$".into()),
            },
        );
        let resolved = resolve_grammar_constrained_sampling(&t, true)
            .unwrap()
            .unwrap();
        assert_eq!(resolved.syntax, GrammarSyntax::Lark);
        assert_eq!(resolved.definition, "start: /[a-z]+/");
        assert_eq!(resolved.input_property, "code");
    }

    #[test]
    fn resolve_grammar_falls_back_to_regex_when_lark_blank() {
        let t = grammar_tool(
            "x",
            GrammarVariants {
                openai_lark: Some("   ".into()),
                openai_regex: Some("^[a-z]+$".into()),
            },
        );
        let resolved = resolve_grammar_constrained_sampling(&t, true)
            .unwrap()
            .unwrap();
        assert_eq!(resolved.syntax, GrammarSyntax::Regex);
        assert_eq!(resolved.definition, "^[a-z]+$");
    }

    #[test]
    fn resolve_grammar_v7_provider_unsupported_falls_back_without_error() {
        let t = grammar_tool(
            "x",
            GrammarVariants {
                openai_lark: Some("start: /[a-z]+/".into()),
                openai_regex: None,
            },
        );
        assert_eq!(
            resolve_grammar_constrained_sampling(&t, false).unwrap(),
            None
        );
    }

    #[test]
    fn resolve_grammar_rejects_all_blank_variants() {
        let t = grammar_tool(
            "x",
            GrammarVariants {
                openai_lark: Some("  ".into()),
                openai_regex: Some(String::new()),
            },
        );
        let error = resolve_grammar_constrained_sampling(&t, true).unwrap_err();
        assert_eq!(
            error.to_string(),
            "Tool \"x\" cannot use grammar constrained sampling: no supported grammar variant was provided."
        );
    }

    #[test]
    fn resolve_grammar_requires_object_parameter_schema() {
        let mut t = grammar_tool(
            "x",
            GrammarVariants {
                openai_lark: Some("start: /[a-z]+/".into()),
                openai_regex: None,
            },
        );
        t.parameters = json!({"type": "string"});
        let error = resolve_grammar_constrained_sampling(&t, true).unwrap_err();
        assert_eq!(
            error.to_string(),
            "Tool \"x\" cannot use grammar constrained sampling: grammar constrained sampling requires an object parameter schema."
        );
    }

    #[test]
    fn resolve_grammar_requires_exactly_one_required_property() {
        let mut t = grammar_tool(
            "x",
            GrammarVariants {
                openai_lark: Some("start: /[a-z]+/".into()),
                openai_regex: None,
            },
        );
        t.parameters = json!({
            "type": "object",
            "properties": {"a": {"type": "string"}, "b": {"type": "string"}},
            "required": ["a", "b"]
        });
        let error = resolve_grammar_constrained_sampling(&t, true).unwrap_err();
        assert!(
            error
                .to_string()
                .contains("exactly one required string property")
        );
    }

    #[test]
    fn resolve_grammar_requires_matching_properties_entry() {
        let mut t = grammar_tool(
            "x",
            GrammarVariants {
                openai_lark: Some("start: /[a-z]+/".into()),
                openai_regex: None,
            },
        );
        t.parameters = json!({
            "type": "object",
            "properties": {"other": {"type": "string"}},
            "required": ["missing"]
        });
        let error = resolve_grammar_constrained_sampling(&t, true).unwrap_err();
        assert!(
            error
                .to_string()
                .contains("requires a properties entry for missing")
        );
    }

    #[test]
    fn resolve_grammar_requires_string_typed_property() {
        let mut t = grammar_tool(
            "x",
            GrammarVariants {
                openai_lark: Some("start: /[a-z]+/".into()),
                openai_regex: None,
            },
        );
        t.parameters = json!({
            "type": "object",
            "properties": {"code": {"type": "number"}},
            "required": ["code"]
        });
        let error = resolve_grammar_constrained_sampling(&t, true).unwrap_err();
        assert!(
            error
                .to_string()
                .contains("property code must have type string")
        );
    }

    // ----- grammar_tool_input_properties / grammar_tool_input -----

    #[test]
    fn grammar_tool_input_properties_empty_when_no_tools() {
        assert!(
            grammar_tool_input_properties(None, true)
                .unwrap()
                .is_empty()
        );
    }

    #[test]
    fn grammar_tool_input_properties_maps_only_grammar_tools() {
        let grammar = grammar_tool(
            "g",
            GrammarVariants {
                openai_lark: Some("start: /[a-z]+/".into()),
                openai_regex: None,
            },
        );
        let plain = tool("p", json!({"type": "object"}));
        let map = grammar_tool_input_properties(Some(&[grammar, plain]), true).unwrap();
        assert_eq!(map.len(), 1);
        assert_eq!(map.get("g"), Some(&"code".to_owned()));
    }

    #[test]
    fn grammar_tool_input_returns_string_argument() {
        let args = Map::from_iter([("code".to_owned(), json!("hello"))]);
        assert_eq!(grammar_tool_input("g", &args, "code").unwrap(), "hello");
    }

    #[test]
    fn grammar_tool_input_errors_on_missing_argument() {
        let args = Map::new();
        let error = grammar_tool_input("g", &args, "code").unwrap_err();
        assert_eq!(
            error.to_string(),
            "Grammar tool call \"g\" requires argument \"code\" to be a string."
        );
    }

    #[test]
    fn grammar_tool_input_errors_on_non_string_argument() {
        let args = Map::from_iter([("code".to_owned(), json!(42))]);
        assert!(grammar_tool_input("g", &args, "code").is_err());
    }

    // ----- GrammarToolInputBuffer -----

    #[test]
    fn buffer_opens_with_property_key_and_partial_quote() {
        let mut buffer = GrammarToolInputBuffer::default();
        let fragment = buffer.append("code", "hello", false).unwrap().unwrap();
        assert_eq!(fragment, "{\"code\":\"hello");
    }

    #[test]
    fn buffer_delta_is_only_the_new_suffix() {
        let mut buffer = GrammarToolInputBuffer::default();
        buffer.append("code", "he", false).unwrap();
        let fragment = buffer.append("code", "hello", false).unwrap().unwrap();
        assert_eq!(fragment, "llo");
    }

    #[test]
    fn buffer_suppresses_empty_non_closing_delta() {
        let mut buffer = GrammarToolInputBuffer::default();
        buffer.append("code", "hello", false).unwrap();
        assert_eq!(buffer.append("code", "hello", false).unwrap(), None);
    }

    #[test]
    fn buffer_close_with_no_new_bytes_emits_terminator_only() {
        let mut buffer = GrammarToolInputBuffer::default();
        buffer.append("code", "hello", false).unwrap();
        let fragment = buffer.append("code", "hello", true).unwrap().unwrap();
        assert_eq!(fragment, "\"}");
    }

    #[test]
    fn buffer_close_on_first_call_emits_full_object() {
        let mut buffer = GrammarToolInputBuffer::default();
        let fragment = buffer.append("code", "", true).unwrap().unwrap();
        assert_eq!(fragment, "{\"code\":\"\"}");
        let parsed: Value = serde_json::from_str(&fragment).unwrap();
        assert_eq!(parsed, json!({"code": ""}));
    }

    #[test]
    fn buffer_v8_embedded_quote_streams_and_concatenates_to_valid_json() {
        let mut buffer = GrammarToolInputBuffer::default();
        let opening = buffer.append("input", "he\"llo", false).unwrap().unwrap();
        assert_eq!(opening, "{\"input\":\"he\\\"llo");
        let closing = buffer.append("input", "he\"llo", true).unwrap().unwrap();
        assert_eq!(closing, "\"}");
        let parsed: Value = serde_json::from_str(&(opening + &closing)).unwrap();
        assert_eq!(parsed, json!({"input": "he\"llo"}));
    }

    #[test]
    fn buffer_v9_non_monotonic_input_is_rejected() {
        let mut buffer = GrammarToolInputBuffer::default();
        buffer.append("input", "he", false).unwrap();
        let error = buffer.append("input", "xy", false).unwrap_err();
        assert_eq!(
            error.to_string(),
            "grammar tool input for property \"input\" changed non-monotonically"
        );
    }

    #[test]
    fn buffer_idempotent_close_returns_none() {
        let mut buffer = GrammarToolInputBuffer::default();
        buffer.append("code", "hello", true).unwrap();
        assert_eq!(buffer.append("code", "hello", true).unwrap(), None);
    }

    #[test]
    fn buffer_change_after_close_is_rejected() {
        let mut buffer = GrammarToolInputBuffer::default();
        buffer.append("code", "hello", true).unwrap();
        let error = buffer.append("code", "hello world", true).unwrap_err();
        assert_eq!(
            error.to_string(),
            "grammar tool input for property \"code\" changed after it was closed"
        );
    }

    #[test]
    fn buffer_reopen_after_close_without_close_flag_is_rejected() {
        let mut buffer = GrammarToolInputBuffer::default();
        buffer.append("code", "hello", true).unwrap();
        assert!(buffer.append("code", "hello", false).is_err());
    }
}
