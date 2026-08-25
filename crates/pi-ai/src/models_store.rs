//! Persistent dynamic model catalogs (`models-store.json`) and override composition.
//!
//! [`ModelsStore`] is the object-safe persistence boundary for provider-scoped
//! dynamic catalogs. [`FileModelsStore`] reuses [`FileLockBackend`] for
//! whole-file locked read-modify-write with atomic replace. Unknown top-level
//! provider keys and unknown fields on store entries / models survive RMW.
//!
//! Tolerant per-model overrides (the `models.json` `modelOverrides` layer) are
//! applied through [`crate::catalog::effective_models`] / [`apply_model_overrides`]
//! and never mutate built-ins or rewrite store source bytes on validation
//! failure.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use futures::future::BoxFuture;
use serde_json::{Map, Value};
use tokio::sync::Mutex;

use crate::auth::error::{ModelsError, ModelsErrorCode, StoreError};
use crate::auth::file_store::FileLockBackend;
use crate::catalog::{
    BuiltinModels, CatalogError, ModelsStoreEntry, apply_model_override, effective_models,
    model_from_value,
};
use crate::types::Model;

/// Per-model JSON overrides as carried by `models.json` `modelOverrides`.
///
/// Keys are model ids. Values are partial model objects merged via
/// [`crate::catalog::apply_model_override`].
pub type ModelOverrides = BTreeMap<String, Value>;

/// Persistent model catalogs keyed by provider ID.
pub trait ModelsStore: Send + Sync {
    /// Read one provider's stored catalog entry.
    ///
    /// Returns `Ok(None)` when the provider key is absent. An explicit empty
    /// `models` list is returned as `Ok(Some(entry))` and replaces built-ins
    /// when composed through [`effective_models`].
    fn read<'a>(
        &'a self,
        provider_id: &'a str,
    ) -> BoxFuture<'a, Result<Option<ModelsStoreEntry>, StoreError>>;

    /// Replace one provider's stored catalog entry.
    ///
    /// Invalid required model fields fail with a path-rich error and leave the
    /// on-disk file unchanged.
    fn write<'a>(
        &'a self,
        provider_id: &'a str,
        entry: ModelsStoreEntry,
    ) -> BoxFuture<'a, Result<(), StoreError>>;

    /// Remove one provider's stored catalog entry.
    fn delete<'a>(&'a self, provider_id: &'a str) -> BoxFuture<'a, Result<(), StoreError>>;
}

impl<T> ModelsStore for Arc<T>
where
    T: ModelsStore + ?Sized,
{
    fn read<'a>(
        &'a self,
        provider_id: &'a str,
    ) -> BoxFuture<'a, Result<Option<ModelsStoreEntry>, StoreError>> {
        (**self).read(provider_id)
    }

    fn write<'a>(
        &'a self,
        provider_id: &'a str,
        entry: ModelsStoreEntry,
    ) -> BoxFuture<'a, Result<(), StoreError>> {
        (**self).write(provider_id, entry)
    }

    fn delete<'a>(&'a self, provider_id: &'a str) -> BoxFuture<'a, Result<(), StoreError>> {
        (**self).delete(provider_id)
    }
}

/// [`ModelsStore`] scoped to one provider. Providers cannot access other
/// providers' catalogs through this surface.
pub trait ProviderModelsStore: Send + Sync {
    /// Read this provider's stored catalog entry.
    fn read(&self) -> BoxFuture<'_, Result<Option<ModelsStoreEntry>, StoreError>>;

    /// Replace this provider's stored catalog entry.
    fn write(&self, entry: ModelsStoreEntry) -> BoxFuture<'_, Result<(), StoreError>>;

    /// Remove this provider's stored catalog entry.
    fn delete(&self) -> BoxFuture<'_, Result<(), StoreError>>;
}

/// In-memory [`ModelsStore`] that clones on every read/write (structuredClone).
#[derive(Clone, Default)]
pub struct InMemoryModelsStore {
    entries: Arc<Mutex<BTreeMap<String, ModelsStoreEntry>>>,
}

impl InMemoryModelsStore {
    /// Create an empty in-memory store.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }
}

impl ModelsStore for InMemoryModelsStore {
    fn read<'a>(
        &'a self,
        provider_id: &'a str,
    ) -> BoxFuture<'a, Result<Option<ModelsStoreEntry>, StoreError>> {
        Box::pin(async move {
            let entries = self.entries.lock().await;
            Ok(entries.get(provider_id).cloned())
        })
    }

    fn write<'a>(
        &'a self,
        provider_id: &'a str,
        entry: ModelsStoreEntry,
    ) -> BoxFuture<'a, Result<(), StoreError>> {
        Box::pin(async move {
            validate_models_store_entry(&entry, &format!("modelsStore.{provider_id}"))?;
            let mut entries = self.entries.lock().await;
            entries.insert(provider_id.to_owned(), entry);
            Ok(())
        })
    }

    fn delete<'a>(&'a self, provider_id: &'a str) -> BoxFuture<'a, Result<(), StoreError>> {
        Box::pin(async move {
            let mut entries = self.entries.lock().await;
            entries.remove(provider_id);
            Ok(())
        })
    }
}

/// Locked JSON-backed storage for dynamically refreshed provider catalogs.
///
/// File shape matches TypeScript `models-store.json`:
/// `{ "<providerId>": { "models": [Model, ...], "checkedAt"?: number, ... } }`.
#[derive(Clone, Debug)]
pub struct FileModelsStore {
    backend: FileLockBackend,
}

impl FileModelsStore {
    /// Create a file-backed models store at `path` (`models-store.json`).
    ///
    /// # Errors
    ///
    /// Returns [`StoreError`] when the canonical lock target cannot be
    /// resolved (symlink cycles, inspection failures).
    pub fn new(path: impl Into<PathBuf>) -> Result<Self, StoreError> {
        Ok(Self {
            backend: FileLockBackend::new(path)?,
        })
    }

    /// Path of the underlying `models-store.json` file.
    #[must_use]
    pub fn path(&self) -> &Path {
        self.backend.path()
    }

    /// Shared lock/atomic backend.
    #[must_use]
    pub fn backend(&self) -> &FileLockBackend {
        &self.backend
    }
}

impl ModelsStore for FileModelsStore {
    fn read<'a>(
        &'a self,
        provider_id: &'a str,
    ) -> BoxFuture<'a, Result<Option<ModelsStoreEntry>, StoreError>> {
        Box::pin(async move {
            let path = self.backend.path().to_path_buf();
            self.backend
                .with_lock_async(move |content| async move {
                    let root = parse_models_store_root(content.as_deref(), &path)?;
                    let Some(value) = root.get(provider_id) else {
                        return Ok((None, None));
                    };
                    let entry = models_store_entry_from_value(
                        value,
                        &format!("{}.{provider_id}", path.display()),
                    )?;
                    Ok((Some(entry), None))
                })
                .await
        })
    }

    fn write<'a>(
        &'a self,
        provider_id: &'a str,
        entry: ModelsStoreEntry,
    ) -> BoxFuture<'a, Result<(), StoreError>> {
        Box::pin(async move {
            let path = self.backend.path().to_path_buf();
            let provider_id = provider_id.to_owned();
            self.backend
                .with_lock_async(move |content| async move {
                    // Validate before mutating the parsed map so a bad entry
                    // never rewrites source bytes.
                    validate_models_store_entry(
                        &entry,
                        &format!("{}.{provider_id}", path.display()),
                    )?;

                    let mut root = parse_models_store_root(content.as_deref(), &path)?;
                    let previous = root.get(&provider_id).cloned();
                    let next_entry = models_store_entry_to_value(&entry, previous.as_ref())?;
                    root.insert(provider_id, next_entry);
                    let next = serialize_models_store_root(&root)?;
                    Ok(((), Some(next)))
                })
                .await
        })
    }

    fn delete<'a>(&'a self, provider_id: &'a str) -> BoxFuture<'a, Result<(), StoreError>> {
        Box::pin(async move {
            let path = self.backend.path().to_path_buf();
            let provider_id = provider_id.to_owned();
            self.backend
                .with_lock_async(move |content| async move {
                    let mut root = parse_models_store_root(content.as_deref(), &path)?;
                    if root.remove(&provider_id).is_none() {
                        // No-op delete still succeeds; leave bytes unchanged.
                        return Ok(((), None));
                    }
                    let next = serialize_models_store_root(&root)?;
                    Ok(((), Some(next)))
                })
                .await
        })
    }
}

/// Provider-scoped view over any [`ModelsStore`].
#[derive(Clone, Debug)]
pub struct ScopedModelsStore<S> {
    inner: S,
    provider_id: String,
}

impl<S> ScopedModelsStore<S> {
    /// Scope `inner` to a single provider id.
    #[must_use]
    pub fn new(inner: S, provider_id: impl Into<String>) -> Self {
        Self {
            inner,
            provider_id: provider_id.into(),
        }
    }

    /// Provider id this store is scoped to.
    #[must_use]
    pub fn provider_id(&self) -> &str {
        &self.provider_id
    }

    /// Borrow the underlying store.
    #[must_use]
    pub fn inner(&self) -> &S {
        &self.inner
    }
}

impl<S> ProviderModelsStore for ScopedModelsStore<S>
where
    S: ModelsStore,
{
    fn read(&self) -> BoxFuture<'_, Result<Option<ModelsStoreEntry>, StoreError>> {
        self.inner.read(&self.provider_id)
    }

    fn write(&self, entry: ModelsStoreEntry) -> BoxFuture<'_, Result<(), StoreError>> {
        self.inner.write(&self.provider_id, entry)
    }

    fn delete(&self) -> BoxFuture<'_, Result<(), StoreError>> {
        self.inner.delete(&self.provider_id)
    }
}

/// Apply per-model overrides (models.json `modelOverrides`) onto a model list.
///
/// Unknown model ids in `overrides` are ignored. Built-in/store models are not
/// mutated in place — each match is cloned through value-level merge.
///
/// # Errors
///
/// Returns [`CatalogError`] when an override produces an invalid model.
pub fn apply_model_overrides(
    models: &[Model],
    overrides: &ModelOverrides,
) -> Result<Vec<Model>, CatalogError> {
    let mut out = Vec::with_capacity(models.len());
    for model in models {
        if let Some(override_val) = overrides.get(&model.id) {
            out.push(apply_model_override(model, override_val)?);
        } else {
            out.push(model.clone());
        }
    }
    Ok(out)
}

/// Compose the effective model list for a provider.
///
/// Precedence matches the catalog merge contract:
/// 1. explicit store entry (including empty `models`) replaces built-ins
/// 2. missing store entry falls back to built-ins for `provider_id`
/// 3. `model_overrides` apply last
///
/// # Errors
///
/// Returns [`CatalogError`] when the selected model source or an override is invalid.
pub fn compose_provider_models(
    provider_id: &str,
    builtins: &BuiltinModels,
    store: Option<&ModelsStoreEntry>,
    model_overrides: &ModelOverrides,
) -> Result<Vec<Model>, CatalogError> {
    effective_models(provider_id, builtins, store, model_overrides)
}

/// Lift a catalog failure into the shared [`ModelsError`] surface.
#[must_use]
pub fn models_error_from_catalog(error: &CatalogError) -> ModelsError {
    let code = match &error {
        CatalogError::Parse { .. } => ModelsErrorCode::ModelSource,
        CatalogError::Validation { .. } => ModelsErrorCode::ModelValidation,
    };
    ModelsError::new(code, error.to_string())
}

/// Lift a store failure into the shared [`ModelsError`] surface.
#[must_use]
pub fn models_error_from_store(error: &StoreError) -> ModelsError {
    ModelsError::new(ModelsErrorCode::ModelSource, error.to_string())
}

fn validate_models_store_entry(entry: &ModelsStoreEntry, path: &str) -> Result<(), StoreError> {
    for (index, model) in entry.models.iter().enumerate() {
        let model_path = format!("{path}.models[{index}]");
        // Re-encode/decode so required-field and unknown-field handling share
        // the catalog path-rich validation path.
        let value = serde_json::to_value(model).map_err(|error| {
            StoreError::message(format!("{model_path}: failed to encode model: {error}"))
        })?;
        model_from_value(&value, &model_path).map_err(|error| catalog_error_to_store(&error))?;
    }
    Ok(())
}

fn catalog_error_to_store(error: &CatalogError) -> StoreError {
    StoreError::message(error.to_string())
}

fn parse_models_store_root(
    content: Option<&str>,
    file_path: &Path,
) -> Result<Map<String, Value>, StoreError> {
    let Some(content) = content else {
        return Ok(Map::new());
    };
    let trimmed = content.trim();
    if trimmed.is_empty() {
        return Ok(Map::new());
    }

    let value: Value = serde_json::from_str(trimmed).map_err(|error| {
        StoreError::message(format!("{}: invalid JSON: {error}", file_path.display()))
    })?;

    match value {
        Value::Object(map) => Ok(map),
        other => Err(StoreError::message(format!(
            "{}: root must be a JSON object, got {other}",
            file_path.display()
        ))),
    }
}

fn serialize_models_store_root(root: &Map<String, Value>) -> Result<String, StoreError> {
    // Pretty 2-space indent matches JSON.stringify(_, null, 2).
    let value = Value::Object(root.clone());
    serde_json::to_string_pretty(&value)
        .map_err(|error| StoreError::message(format!("Failed to serialize models store: {error}")))
}

fn models_store_entry_from_value(
    value: &Value,
    path: &str,
) -> Result<ModelsStoreEntry, StoreError> {
    let object = value
        .as_object()
        .ok_or_else(|| StoreError::message(format!("{path}: entry must be a JSON object")))?;

    let models_value = object
        .get("models")
        .ok_or_else(|| StoreError::message(format!("{path}.models: required field missing")))?;
    let models_array = models_value
        .as_array()
        .ok_or_else(|| StoreError::message(format!("{path}.models: must be a JSON array")))?;

    let mut models = Vec::with_capacity(models_array.len());
    for (index, model_value) in models_array.iter().enumerate() {
        let model_path = format!("{path}.models[{index}]");
        let model = model_from_value(model_value, &model_path)
            .map_err(|error| catalog_error_to_store(&error))?;
        models.push(model);
    }

    let checked_at = match object.get("checkedAt") {
        None | Some(Value::Null) => None,
        Some(Value::Number(number)) => number
            .as_i64()
            .or_else(|| number.as_u64().and_then(|n| i64::try_from(n).ok()))
            .ok_or_else(|| {
                StoreError::message(format!("{path}.checkedAt: must be a finite JSON number"))
            })
            .map(Some)?,
        Some(other) => {
            return Err(StoreError::message(format!(
                "{path}.checkedAt: must be a JSON number, got {other}"
            )));
        }
    };

    Ok(ModelsStoreEntry { models, checked_at })
}

fn models_store_entry_to_value(
    entry: &ModelsStoreEntry,
    previous: Option<&Value>,
) -> Result<Value, StoreError> {
    let mut object = match previous {
        Some(Value::Object(map)) => map.clone(),
        _ => Map::new(),
    };

    let models = serde_json::to_value(&entry.models).map_err(|error| {
        StoreError::message(format!("Failed to encode models store entry: {error}"))
    })?;
    object.insert("models".to_owned(), models);

    match entry.checked_at {
        Some(checked_at) => {
            object.insert("checkedAt".to_owned(), Value::from(checked_at));
        }
        None => {
            object.remove("checkedAt");
        }
    }

    Ok(Value::Object(object))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::{ModelCost, ModelInput};
    use serde_json::json;
    use std::fs;
    use std::sync::Arc;
    use std::time::Duration;
    use tempfile::tempdir;
    type TestResult = Result<(), Box<dyn std::error::Error + Send + Sync>>;

    fn missing_entry(provider_id: &str) -> std::io::Error {
        std::io::Error::other(format!("missing models-store entry for {provider_id}"))
    }

    fn sample_model(id: &str, provider: &str) -> Model {
        Model {
            id: id.to_owned(),
            name: format!("Name {id}"),
            api: "openai-completions".to_owned(),
            provider: provider.to_owned(),
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
            compat: Some(json!({"supportsDeveloperRole": false})),
            extra: BTreeMap::from([(
                "futureField".to_owned(),
                json!({"nested": {"keep": true}, "flag": 1}),
            )]),
        }
    }

    #[tokio::test]
    async fn in_memory_read_is_isolated_clone() -> TestResult {
        let store = InMemoryModelsStore::new();
        let mut entry = ModelsStoreEntry {
            models: vec![sample_model("demo-1", "demo")],
            checked_at: Some(1_700_000_000_000),
        };
        store.write("demo", entry.clone()).await?;

        let mut read = store
            .read("demo")
            .await?
            .ok_or_else(|| missing_entry("demo"))?;
        read.models[0].name = "mutated".to_owned();
        read.checked_at = Some(0);

        let again = store
            .read("demo")
            .await?
            .ok_or_else(|| missing_entry("demo"))?;
        assert_eq!(again.models[0].name, entry.models[0].name);
        assert_eq!(again.checked_at, entry.checked_at);

        // Keep entry used so the clone assertion is against the original write.
        entry.models[0].name.push_str("-still");
        let third = store
            .read("demo")
            .await?
            .ok_or_else(|| missing_entry("demo"))?;
        assert_eq!(third.models[0].name, "Name demo-1");
        Ok(())
    }

    #[tokio::test]
    async fn file_store_preserves_unknown_entry_and_model_fields() -> TestResult {
        let dir = tempdir()?;
        let path = dir.path().join("models-store.json");
        let raw = r#"{
  "openrouter": {
    "models": [
      {
        "id": "or/one",
        "name": "One",
        "api": "openai-completions",
        "provider": "openrouter",
        "baseUrl": "https://openrouter.ai/api/v1",
        "reasoning": false,
        "input": ["text"],
        "cost": {
          "input": 1.0,
          "output": 2.0,
          "cacheRead": 0.0,
          "cacheWrite": 0.0
        },
        "contextWindow": 1000,
        "maxTokens": 100,
        "futureField": {
          "nested": {
            "keep": true
          },
          "flag": 1
        },
        "brandNewModelKey": "stay"
      }
    ],
    "checkedAt": 1700000000000,
    "refreshHint": "preserve-me",
    "nestedMeta": {
      "a": 1
    }
  },
  "other": {
    "models": [],
    "checkedAt": 1,
    "topLevelOnly": true
  }
}"#;
        fs::write(&path, raw)?;

        let store = FileModelsStore::new(&path)?;
        let entry = store
            .read("openrouter")
            .await?
            .ok_or_else(|| missing_entry("openrouter"))?;
        assert_eq!(entry.checked_at, Some(1_700_000_000_000));
        assert_eq!(entry.models.len(), 1);
        assert_eq!(
            entry.models[0].extra.get("futureField"),
            Some(&json!({"nested": {"keep": true}, "flag": 1}))
        );
        assert_eq!(
            entry.models[0].extra.get("brandNewModelKey"),
            Some(&json!("stay"))
        );

        // Rewrite the same provider with a name change only; unknown entry keys
        // and sibling providers must remain on disk.
        let mut rewritten = entry.clone();
        rewritten.models[0].name = "One Renamed".to_owned();
        rewritten.checked_at = Some(1_700_000_000_001);
        store.write("openrouter", rewritten).await?;

        let on_disk: Value = serde_json::from_str(&fs::read_to_string(&path)?)?;
        assert_eq!(on_disk["openrouter"]["refreshHint"], json!("preserve-me"));
        assert_eq!(on_disk["openrouter"]["nestedMeta"]["a"], json!(1));
        assert_eq!(
            on_disk["openrouter"]["checkedAt"],
            Value::from(1_700_000_000_001_i64)
        );
        assert_eq!(
            on_disk["openrouter"]["models"][0]["futureField"]["nested"]["keep"],
            json!(true)
        );
        assert_eq!(
            on_disk["openrouter"]["models"][0]["brandNewModelKey"],
            json!("stay")
        );
        assert_eq!(
            on_disk["openrouter"]["models"][0]["name"],
            json!("One Renamed")
        );
        assert_eq!(on_disk["other"]["topLevelOnly"], json!(true));
        assert_eq!(on_disk["other"]["models"], json!([]));
        Ok(())
    }

    #[tokio::test]
    async fn file_store_malformed_json_does_not_rewrite_source() -> TestResult {
        let dir = tempdir()?;
        let path = dir.path().join("models-store.json");
        let bad = "{ not-json ";
        fs::write(&path, bad)?;

        let store = FileModelsStore::new(&path)?;
        let Err(err) = store.read("demo").await else {
            return Err("malformed models store unexpectedly parsed".into());
        };
        let message = err.to_string();
        assert!(
            message.contains(&*path.to_string_lossy()),
            "error should mention path: {message}"
        );
        assert!(
            message.contains("invalid JSON") || message.contains("expected"),
            "error should be parse-related: {message}"
        );

        let after = fs::read_to_string(&path)?;
        assert_eq!(after, bad, "malformed source must not be rewritten");

        let Err(write_err) = store
            .write(
                "demo",
                ModelsStoreEntry {
                    models: vec![sample_model("demo-1", "demo")],
                    checked_at: Some(1),
                },
            )
            .await
        else {
            return Err("write unexpectedly replaced malformed source".into());
        };
        assert!(write_err.to_string().contains("invalid JSON"));
        let after_write = fs::read_to_string(&path)?;
        assert_eq!(after_write, bad, "failed write must not rewrite source");
        Ok(())
    }

    #[tokio::test]
    async fn file_store_invalid_model_fields_are_path_rich_and_no_rewrite() -> TestResult {
        let dir = tempdir()?;
        let path = dir.path().join("models-store.json");
        let good = r#"{
  "demo": {
    "models": [],
    "checkedAt": 9
  }
}"#;
        fs::write(&path, good)?;

        let store = FileModelsStore::new(&path)?;
        let mut bad_model = sample_model("demo-1", "demo");
        bad_model.name = "   ".to_owned();
        let Err(err) = store
            .write(
                "demo",
                ModelsStoreEntry {
                    models: vec![bad_model],
                    checked_at: Some(10),
                },
            )
            .await
        else {
            return Err("invalid model write unexpectedly succeeded".into());
        };
        let message = err.to_string();
        assert!(
            message.contains(".models[0].name") || message.contains("required field is empty"),
            "path-rich validation expected, got: {message}"
        );

        let after = fs::read_to_string(&path)?;
        assert_eq!(after, good, "validation failure must not rewrite source");
        Ok(())
    }

    #[tokio::test]
    async fn empty_store_list_replaces_builtins_missing_provider_falls_back() -> TestResult {
        let builtin = sample_model("demo-1", "demo");
        let mut builtins = BuiltinModels::new();
        builtins.insert(
            "demo".to_owned(),
            BTreeMap::from([(builtin.id.clone(), builtin.clone())]),
        );

        let store = InMemoryModelsStore::new();
        store
            .write(
                "demo",
                ModelsStoreEntry {
                    models: Vec::new(),
                    checked_at: Some(42),
                },
            )
            .await?;

        let entry = store
            .read("demo")
            .await?
            .ok_or_else(|| missing_entry("demo"))?;
        let effective = compose_provider_models("demo", &builtins, Some(&entry), &BTreeMap::new())?;
        assert!(
            effective.is_empty(),
            "explicit empty store list must replace builtins"
        );

        let missing_store = store.read("other").await?;
        assert!(missing_store.is_none());
        let fallback =
            compose_provider_models("demo", &builtins, missing_store.as_ref(), &BTreeMap::new())?;
        assert_eq!(fallback, vec![builtin]);

        let unknown_provider = compose_provider_models("nope", &builtins, None, &BTreeMap::new())?;
        assert!(unknown_provider.is_empty());
        Ok(())
    }

    #[tokio::test]
    async fn override_precedence_applies_after_store_and_preserves_unknowns() -> TestResult {
        let builtin = sample_model("demo-1", "demo");
        let mut store_model = sample_model("store-only", "demo");
        store_model.name = "From Store".to_owned();

        let mut builtins = BuiltinModels::new();
        builtins.insert(
            "demo".to_owned(),
            BTreeMap::from([(builtin.id.clone(), builtin)]),
        );

        let store = InMemoryModelsStore::new();
        store
            .write(
                "demo",
                ModelsStoreEntry {
                    models: vec![store_model.clone()],
                    checked_at: None,
                },
            )
            .await?;

        let entry = store
            .read("demo")
            .await?
            .ok_or_else(|| missing_entry("demo"))?;
        let overrides = BTreeMap::from([(
            "store-only".to_owned(),
            json!({
                "name": "Overridden",
                "futureField": {"nested": {"extra": 2}, "flag": 2},
                "brandNew": true
            }),
        )]);

        let effective = compose_provider_models("demo", &builtins, Some(&entry), &overrides)?;
        assert_eq!(effective.len(), 1);
        assert_eq!(effective[0].id, "store-only");
        assert_eq!(effective[0].name, "Overridden");
        assert_eq!(
            effective[0].extra["futureField"]["nested"]["keep"],
            json!(true)
        );
        assert_eq!(
            effective[0].extra["futureField"]["nested"]["extra"],
            json!(2)
        );
        assert_eq!(effective[0].extra["futureField"]["flag"], json!(2));
        assert_eq!(effective[0].extra["brandNew"], json!(true));

        // Built-ins untouched; store entry still original name.
        let again = store
            .read("demo")
            .await?
            .ok_or_else(|| missing_entry("demo"))?;
        assert_eq!(again.models[0].name, "From Store");
        Ok(())
    }

    #[tokio::test]
    async fn concurrent_file_writers_serialize_without_corruption() -> TestResult {
        let dir = tempdir()?;
        let path = dir.path().join("models-store.json");
        // Pre-seed empty object so both writers share one path.
        fs::write(&path, "{}")?;

        let path_a = path.clone();
        let path_b = path.clone();
        let a = tokio::spawn(async move {
            let store = FileModelsStore::new(path_a)?;
            for i in 0..20 {
                store
                    .write(
                        "alpha",
                        ModelsStoreEntry {
                            models: vec![sample_model(&format!("a-{i}"), "alpha")],
                            checked_at: Some(i64::from(i)),
                        },
                    )
                    .await?;
            }
            Ok::<(), StoreError>(())
        });
        let b = tokio::spawn(async move {
            let store = FileModelsStore::new(path_b)?;
            for i in 0..20 {
                store
                    .write(
                        "beta",
                        ModelsStoreEntry {
                            models: vec![sample_model(&format!("b-{i}"), "beta")],
                            checked_at: Some(100 + i64::from(i)),
                        },
                    )
                    .await?;
            }
            Ok::<(), StoreError>(())
        });

        a.await??;
        b.await??;

        // Give the filesystem a beat on slow hosts; lock should already serialize.
        tokio::time::sleep(Duration::from_millis(10)).await;

        let on_disk = fs::read_to_string(&path)?;
        let root: Value = serde_json::from_str(&on_disk)?;
        assert!(root.get("alpha").is_some(), "alpha entry present");
        assert!(root.get("beta").is_some(), "beta entry present");
        assert!(
            root["alpha"]["models"]
                .as_array()
                .is_some_and(|m| !m.is_empty())
        );
        assert!(
            root["beta"]["models"]
                .as_array()
                .is_some_and(|m| !m.is_empty())
        );

        let store = FileModelsStore::new(&path)?;
        let alpha = store
            .read("alpha")
            .await?
            .ok_or_else(|| missing_entry("alpha"))?;
        let beta = store
            .read("beta")
            .await?
            .ok_or_else(|| missing_entry("beta"))?;
        assert_eq!(alpha.models.len(), 1);
        assert_eq!(beta.models.len(), 1);
        assert!(alpha.models[0].id.starts_with("a-"));
        assert!(beta.models[0].id.starts_with("b-"));
        Ok(())
    }

    #[tokio::test]
    async fn scoped_store_only_touches_one_provider() -> TestResult {
        let store = Arc::new(InMemoryModelsStore::new());
        store
            .write(
                "keep",
                ModelsStoreEntry {
                    models: vec![sample_model("k", "keep")],
                    checked_at: Some(1),
                },
            )
            .await?;

        let scoped = ScopedModelsStore::new(Arc::clone(&store), "demo");
        scoped
            .write(ModelsStoreEntry {
                models: vec![sample_model("d", "demo")],
                checked_at: Some(2),
            })
            .await?;
        let demo = scoped.read().await?.ok_or_else(|| missing_entry("demo"))?;
        assert_eq!(demo.models[0].id, "d");
        scoped.delete().await?;
        assert!(scoped.read().await?.is_none());
        assert!(store.read("keep").await?.is_some());
        Ok(())
    }

    #[test]
    fn no_runtime_bun_required_for_builtin_compose() -> TestResult {
        // Runtime catalog load is include_str! only — composing with an empty
        // store path must not shell out or require Bun.
        let builtins = crate::catalog::builtin_models()?;
        assert!(
            !builtins.is_empty(),
            "checked-in builtin-models.json must load without Bun"
        );
        let composed =
            compose_provider_models("nonexistent-provider-xyz", builtins, None, &BTreeMap::new())?;
        assert!(composed.is_empty());
        Ok(())
    }

    #[test]
    fn models_error_mapping_preserves_path() {
        let catalog_err = CatalogError::validation("models[0].name", "required field is empty");
        let models_err = models_error_from_catalog(&catalog_err);
        assert_eq!(models_err.code, ModelsErrorCode::ModelValidation);
        assert!(models_err.message().contains("models[0].name"));

        let store_err = StoreError::message("/tmp/models-store.json: invalid JSON: eof");
        let mapped = models_error_from_store(&store_err);
        assert_eq!(mapped.code, ModelsErrorCode::ModelSource);
        assert!(mapped.message().contains("models-store.json"));
    }

    #[test]
    fn checked_at_wire_field_round_trips() -> TestResult {
        let entry = ModelsStoreEntry {
            models: vec![sample_model("m", "p")],
            checked_at: Some(1_700_000_000_000),
        };
        let value = models_store_entry_to_value(&entry, None)?;
        assert_eq!(value["checkedAt"], Value::from(1_700_000_000_000_i64));
        let parsed = models_store_entry_from_value(&value, "entry")?;
        assert_eq!(parsed.checked_at, Some(1_700_000_000_000));

        let without = ModelsStoreEntry {
            models: vec![],
            checked_at: None,
        };
        let value = models_store_entry_to_value(
            &without,
            Some(&json!({"models": [], "checkedAt": 1, "extra": true})),
        )?;
        assert!(value.get("checkedAt").is_none());
        assert_eq!(value["extra"], json!(true));
        Ok(())
    }
}
