//! Value-level deep merge and model override application.

use std::collections::BTreeMap;

use serde_json::Value;

use crate::types::Model;

use super::{BuiltinModels, CatalogError, ModelsStoreEntry, model_from_value};

/// Deep-merge JSON values.
///
/// Object keys merge recursively. Arrays and scalars in `overlay` replace the
/// corresponding `base` value entirely.
#[must_use]
pub fn json_merge(base: Value, overlay: Value) -> Value {
    match (base, overlay) {
        (Value::Object(mut base_map), Value::Object(overlay_map)) => {
            for (key, overlay_value) in overlay_map {
                let merged = match base_map.remove(&key) {
                    Some(base_value) => json_merge(base_value, overlay_value),
                    None => overlay_value,
                };
                base_map.insert(key, merged);
            }
            Value::Object(base_map)
        }
        (_base, overlay) => overlay,
    }
}

/// Apply a JSON model override onto a base model while preserving unknown fields.
///
/// Merge happens at the [`serde_json::Value`] layer so flattened extras and nested
/// open objects survive. Required model fields are re-validated after merge.
///
/// # Errors
///
/// Returns [`CatalogError::Validation`] when the override is not an object or
/// the merged value cannot decode to a model with valid required fields.
pub fn apply_model_override(model: &Model, override_val: &Value) -> Result<Model, CatalogError> {
    let path = format!("model.{}", model.id);
    let overlay = match override_val {
        Value::Object(_) => override_val.clone(),
        Value::Null => {
            return Ok(model.clone());
        }
        other => {
            return Err(CatalogError::validation(
                path,
                format!("model override must be a JSON object, got {other}"),
            ));
        }
    };

    let base = serde_json::to_value(model).map_err(|error| {
        CatalogError::validation(path.clone(), format!("failed to encode model: {error}"))
    })?;
    let merged = json_merge(base, overlay);
    let applied = model_from_value(&merged, &path)?;
    Ok(applied)
}

/// Resolve the effective model list for a provider.
///
/// Precedence:
/// 1. When `store` is `Some`, use that entry's models (including an empty list).
/// 2. Otherwise use built-in models for `provider_id` when present.
/// 3. Apply per-model overrides from `overrides` last (keyed by model id).
///
/// Built-ins are never mutated. A missing provider with no store entry yields an
/// empty list.
///
/// # Errors
///
/// Returns the model validation error from an override applied to any
/// effective model.
pub fn effective_models(
    provider_id: &str,
    builtins: &BuiltinModels,
    store: Option<&ModelsStoreEntry>,
    overrides: &BTreeMap<String, Value>,
) -> Result<Vec<Model>, CatalogError> {
    let base_models: Vec<Model> = if let Some(entry) = store {
        entry.models.clone()
    } else {
        builtins
            .get(provider_id)
            .map(|models| models.values().cloned().collect())
            .unwrap_or_default()
    };

    let mut out = Vec::with_capacity(base_models.len());
    for model in base_models {
        if let Some(override_val) = overrides.get(&model.id) {
            out.push(apply_model_override(&model, override_val)?);
        } else {
            out.push(model);
        }
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::{ModelCost, ModelInput};
    use serde_json::json;

    type TestResult = Result<(), CatalogError>;

    fn assert_float_eq(actual: f64, expected: f64) {
        assert!(
            (actual - expected).abs() <= f64::EPSILON,
            "expected {expected}, got {actual}"
        );
    }

    fn expected_error<T>(result: Result<T, CatalogError>) -> Result<CatalogError, CatalogError> {
        match result {
            Err(error) => Ok(error),
            Ok(_) => Err(CatalogError::validation(
                "test",
                "operation unexpectedly succeeded",
            )),
        }
    }

    fn sample_model() -> Model {
        Model {
            id: "demo-1".to_owned(),
            name: "Demo 1".to_owned(),
            api: "openai-completions".to_owned(),
            provider: "demo".to_owned(),
            base_url: "https://example.test/v1".to_owned(),
            reasoning: false,
            thinking_level_map: None,
            input: vec![ModelInput::Text],
            cost: ModelCost {
                input: 1.0,
                output: 2.0,
                cache_read: 0.1,
                cache_write: 0.2,
                tiers: None,
            },
            context_window: 8_192,
            max_tokens: 1_024,
            headers: None,
            compat: Some(json!({
                "supportsDeveloperRole": false,
                "openRouterRouting": {"order": ["a"]},
            })),
            extra: BTreeMap::from([(
                "futureField".to_owned(),
                json!({"nested": {"keep": true}, "flag": 1}),
            )]),
        }
    }

    #[test]
    fn json_merge_deep_merges_objects_and_replaces_arrays_and_scalars() {
        let base = json!({
            "name": "base",
            "cost": {"input": 1, "output": 2},
            "input": ["text"],
            "compat": {
                "a": 1,
                "nested": {"x": 1, "y": 2}
            }
        });
        let overlay = json!({
            "name": "overlay",
            "cost": {"input": 9},
            "input": ["text", "image"],
            "compat": {
                "b": 2,
                "nested": {"y": 3, "z": 4}
            },
            "newKey": true
        });
        let merged = json_merge(base, overlay);
        assert_eq!(merged["name"], json!("overlay"));
        assert_eq!(merged["cost"]["input"], json!(9));
        assert_eq!(merged["cost"]["output"], json!(2));
        assert_eq!(merged["input"], json!(["text", "image"]));
        assert_eq!(merged["compat"]["a"], json!(1));
        assert_eq!(merged["compat"]["b"], json!(2));
        assert_eq!(merged["compat"]["nested"]["x"], json!(1));
        assert_eq!(merged["compat"]["nested"]["y"], json!(3));
        assert_eq!(merged["compat"]["nested"]["z"], json!(4));
        assert_eq!(merged["newKey"], json!(true));
    }

    #[test]
    fn apply_model_override_preserves_unknown_nested_fields() -> TestResult {
        let model = sample_model();
        let override_val = json!({
            "name": "Renamed",
            "reasoning": true,
            "cost": {"output": 5.5},
            "compat": {
                "supportsDeveloperRole": true,
                "openRouterRouting": {"allowFallbacks": false}
            },
            "futureField": {
                "nested": {"extra": 2},
                "flag": 2
            },
            "brandNew": {"ok": true}
        });

        let applied = apply_model_override(&model, &override_val)?;
        assert_eq!(applied.name, "Renamed");
        assert!(applied.reasoning);
        assert_float_eq(applied.cost.input, 1.0);
        assert_float_eq(applied.cost.output, 5.5);
        assert_float_eq(applied.cost.cache_read, 0.1);
        assert_eq!(
            applied.compat,
            Some(json!({
                "supportsDeveloperRole": true,
                "openRouterRouting": {"order": ["a"], "allowFallbacks": false}
            }))
        );
        assert_eq!(applied.extra["futureField"]["nested"]["keep"], json!(true));
        Ok(())
    }

    #[test]
    fn apply_model_override_rejects_empty_required_field_with_path() -> TestResult {
        let model = sample_model();
        let error = expected_error(apply_model_override(&model, &json!({"name": "   "})))?;
        assert_eq!(error.path(), "model.demo-1.name");
        Ok(())
    }

    #[test]
    fn effective_models_uses_builtins_when_store_missing() -> TestResult {
        let model = sample_model();
        let mut builtins = BuiltinModels::new();
        builtins.insert(
            "demo".to_owned(),
            BTreeMap::from([(model.id.clone(), model.clone())]),
        );

        let effective = effective_models("demo", &builtins, None, &BTreeMap::new())?;
        assert_eq!(effective, vec![model]);

        let missing = effective_models("other", &builtins, None, &BTreeMap::new())?;
        assert!(missing.is_empty());
        Ok(())
    }

    #[test]
    fn effective_models_empty_store_entry_replaces_builtins() -> TestResult {
        let model = sample_model();
        let mut builtins = BuiltinModels::new();
        builtins.insert(
            "demo".to_owned(),
            BTreeMap::from([(model.id.clone(), model)]),
        );
        let store = ModelsStoreEntry {
            models: Vec::new(),
            checked_at: Some(1),
        };

        let effective = effective_models("demo", &builtins, Some(&store), &BTreeMap::new())?;
        assert!(
            effective.is_empty(),
            "explicit empty store list must replace builtins"
        );
        Ok(())
    }

    #[test]
    fn effective_models_applies_overrides_last_without_mutating_builtins() -> TestResult {
        let model = sample_model();
        let mut builtins = BuiltinModels::new();
        builtins.insert(
            "demo".to_owned(),
            BTreeMap::from([(model.id.clone(), model.clone())]),
        );
        let overrides = BTreeMap::from([(model.id.clone(), json!({"name": "Overridden"}))]);

        let effective = effective_models("demo", &builtins, None, &overrides)?;
        assert_eq!(effective.len(), 1);
        assert_eq!(effective[0].name, "Overridden");
        assert_eq!(
            builtins["demo"]["demo-1"].name, "Demo 1",
            "builtins must remain immutable"
        );
        Ok(())
    }

    #[test]
    fn effective_models_prefers_store_models_over_builtins() -> TestResult {
        let builtin = sample_model();
        let mut store_model = sample_model();
        store_model.id = "store-only".to_owned();
        store_model.name = "From Store".to_owned();

        let mut builtins = BuiltinModels::new();
        builtins.insert(
            "demo".to_owned(),
            BTreeMap::from([(builtin.id.clone(), builtin)]),
        );
        let store = ModelsStoreEntry {
            models: vec![store_model.clone()],
            checked_at: None,
        };

        let effective = effective_models("demo", &builtins, Some(&store), &BTreeMap::new())?;
        assert_eq!(effective, vec![store_model]);
        Ok(())
    }
}
