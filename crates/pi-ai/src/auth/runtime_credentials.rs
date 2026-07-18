//! Process-local runtime API-key overlay for a backing [`CredentialStore`].
//!
//! Runtime keys mask `read`/`list` for their provider but are never persisted.
//! `modify` always bypasses the overlay and mutates the backing store only;
//! `delete` clears both the overlay entry and the backing credential.

use std::collections::{BTreeMap, HashMap};
use std::sync::{Arc, Mutex};

use futures::future::BoxFuture;

use super::error::{AuthError, StoreError};
use super::types::{ApiKeyCredential, Credential, CredentialInfo, CredentialKind, CredentialStore};

/// Non-persistent API-key overrides layered over another credential store.
#[derive(Clone)]
pub struct RuntimeCredentials {
    inner: Arc<dyn CredentialStore>,
    overrides: Arc<Mutex<HashMap<String, String>>>,
}

impl RuntimeCredentials {
    /// Wrap `inner` with an empty runtime overlay.
    #[must_use]
    pub fn new(inner: Arc<dyn CredentialStore>) -> Self {
        Self {
            inner,
            overrides: Arc::new(Mutex::new(HashMap::new())),
        }
    }

    /// Set a process-local API key for `provider_id`.
    pub fn set_runtime_api_key(&self, provider_id: impl Into<String>, api_key: impl Into<String>) {
        if let Ok(mut overrides) = self.overrides.lock() {
            overrides.insert(provider_id.into(), api_key.into());
        }
    }

    /// Remove a process-local API key override.
    pub fn remove_runtime_api_key(&self, provider_id: &str) {
        if let Ok(mut overrides) = self.overrides.lock() {
            overrides.remove(provider_id);
        }
    }

    /// Whether a runtime API key override is present for `provider_id`.
    #[must_use]
    pub fn has_runtime_api_key(&self, provider_id: &str) -> bool {
        self.overrides
            .lock()
            .is_ok_and(|overrides| overrides.contains_key(provider_id))
    }

    fn override_for(&self, provider_id: &str) -> Result<Option<String>, StoreError> {
        self.overrides
            .lock()
            .map(|overrides| overrides.get(provider_id).cloned())
            .map_err(|_| StoreError::message("Runtime credential overlay lock poisoned"))
    }

    fn clear_override(&self, provider_id: &str) -> Result<(), StoreError> {
        self.overrides
            .lock()
            .map(|mut overrides| {
                overrides.remove(provider_id);
            })
            .map_err(|_| StoreError::message("Runtime credential overlay lock poisoned"))
    }

    fn override_providers(&self) -> Result<Vec<(String, String)>, StoreError> {
        self.overrides
            .lock()
            .map(|overrides| {
                overrides
                    .iter()
                    .map(|(provider, key)| (provider.clone(), key.clone()))
                    .collect()
            })
            .map_err(|_| StoreError::message("Runtime credential overlay lock poisoned"))
    }
}

impl CredentialStore for RuntimeCredentials {
    fn read<'a>(
        &'a self,
        provider_id: &'a str,
    ) -> BoxFuture<'a, Result<Option<Credential>, StoreError>> {
        Box::pin(async move {
            if let Some(api_key) = self.override_for(provider_id)? {
                return Ok(Some(Credential::ApiKey(ApiKeyCredential {
                    key: Some(api_key),
                    env: None,
                })));
            }
            self.inner.read(provider_id).await
        })
    }

    fn list(&self) -> BoxFuture<'_, Result<Vec<CredentialInfo>, StoreError>> {
        Box::pin(async move {
            let mut entries: BTreeMap<String, CredentialInfo> = self
                .inner
                .list()
                .await?
                .into_iter()
                .map(|entry| (entry.provider_id.clone(), entry))
                .collect();
            for (provider_id, _) in self.override_providers()? {
                entries.insert(
                    provider_id.clone(),
                    CredentialInfo {
                        provider_id,
                        kind: CredentialKind::ApiKey,
                    },
                );
            }
            Ok(entries.into_values().collect())
        })
    }

    fn modify<'a>(
        &'a self,
        provider_id: &'a str,
        f: Box<
            dyn FnOnce(
                    Option<Credential>,
                ) -> BoxFuture<'static, Result<Option<Credential>, AuthError>>
                + Send
                + 'a,
        >,
    ) -> BoxFuture<'a, Result<Option<Credential>, StoreError>> {
        // Bypass the overlay: writes always hit the backing store only.
        self.inner.modify(provider_id, f)
    }

    fn delete<'a>(&'a self, provider_id: &'a str) -> BoxFuture<'a, Result<(), StoreError>> {
        Box::pin(async move {
            self.clear_override(provider_id)?;
            self.inner.delete(provider_id).await
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::auth::credential_store::InMemoryCredentialStore;
    use crate::auth::types::{ApiKeyCredential, OAuthCredential};
    use serde_json::json;
    use std::collections::BTreeMap;

    fn api_key(key: &str) -> Credential {
        Credential::ApiKey(ApiKeyCredential {
            key: Some(key.into()),
            env: None,
        })
    }

    #[tokio::test]
    async fn overlay_masks_read_list_and_delete_clears_both() -> Result<(), StoreError> {
        let inner = Arc::new(InMemoryCredentialStore::new());
        inner
            .modify(
                "anthropic",
                Box::new(|_| {
                    Box::pin(async {
                        Ok(Some(Credential::Oauth(OAuthCredential {
                            refresh: "r".into(),
                            access: "a".into(),
                            expires: 1,
                            extra: BTreeMap::from([("accountId".into(), json!("acct"))]),
                        })))
                    })
                }),
            )
            .await?;

        let runtime = RuntimeCredentials::new(inner.clone());
        runtime.set_runtime_api_key("anthropic", "runtime-key");
        runtime.set_runtime_api_key("openai", "sk-runtime");

        assert!(runtime.has_runtime_api_key("anthropic"));
        assert_eq!(
            runtime.read("anthropic").await?,
            Some(api_key("runtime-key"))
        );
        assert_eq!(runtime.read("openai").await?, Some(api_key("sk-runtime")));

        let mut listed = runtime.list().await?;
        listed.sort_by(|a, b| a.provider_id.cmp(&b.provider_id));
        assert_eq!(
            listed,
            vec![
                CredentialInfo {
                    provider_id: "anthropic".into(),
                    kind: CredentialKind::ApiKey,
                },
                CredentialInfo {
                    provider_id: "openai".into(),
                    kind: CredentialKind::ApiKey,
                },
            ]
        );

        // modify bypasses overlay and mutates backing store only.
        let written = runtime
            .modify(
                "openai",
                Box::new(|_| Box::pin(async { Ok(Some(api_key("sk-persisted"))) })),
            )
            .await?;
        assert_eq!(written, Some(api_key("sk-persisted")));
        // Overlay still masks openai reads.
        assert_eq!(runtime.read("openai").await?, Some(api_key("sk-runtime")));
        assert_eq!(inner.read("openai").await?, Some(api_key("sk-persisted")));

        runtime.delete("anthropic").await?;
        assert!(!runtime.has_runtime_api_key("anthropic"));
        assert_eq!(runtime.read("anthropic").await?, None);
        assert_eq!(inner.read("anthropic").await?, None);

        runtime.remove_runtime_api_key("openai");
        assert_eq!(runtime.read("openai").await?, Some(api_key("sk-persisted")));
        Ok(())
    }
}
