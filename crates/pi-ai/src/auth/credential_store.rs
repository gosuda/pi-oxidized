//! In-memory [`CredentialStore`] with per-provider serialized modify.

use std::collections::BTreeMap;
use std::sync::Arc;

use futures::future::BoxFuture;
use tokio::sync::Mutex;

use super::error::StoreError;
use super::types::{Credential, CredentialInfo, CredentialModifyFn, CredentialStore};

/// Default in-memory credential store.
///
/// Writes are serialized per provider id so concurrent `modify`/`delete` calls
/// on the same provider cannot interleave their read-modify-write sequences.
#[derive(Clone, Default)]
pub struct InMemoryCredentialStore {
    inner: Arc<InMemoryCredentialStoreInner>,
}

#[derive(Default)]
struct InMemoryCredentialStoreInner {
    credentials: Mutex<BTreeMap<String, Credential>>,
    provider_locks: Mutex<BTreeMap<String, Arc<Mutex<()>>>>,
}

impl InMemoryCredentialStore {
    /// Create an empty in-memory store.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    async fn provider_lock(&self, provider_id: &str) -> Arc<Mutex<()>> {
        let mut locks = self.inner.provider_locks.lock().await;
        locks
            .entry(provider_id.to_owned())
            .or_insert_with(|| Arc::new(Mutex::new(())))
            .clone()
    }
}

impl CredentialStore for InMemoryCredentialStore {
    fn read<'a>(
        &'a self,
        provider_id: &'a str,
    ) -> BoxFuture<'a, Result<Option<Credential>, StoreError>> {
        Box::pin(async move {
            let credentials = self.inner.credentials.lock().await;
            Ok(credentials.get(provider_id).cloned())
        })
    }

    fn list(&self) -> BoxFuture<'_, Result<Vec<CredentialInfo>, StoreError>> {
        Box::pin(async move {
            let credentials = self.inner.credentials.lock().await;
            Ok(credentials
                .iter()
                .map(|(provider_id, credential)| CredentialInfo {
                    provider_id: provider_id.clone(),
                    kind: credential.kind(),
                })
                .collect())
        })
    }

    fn modify<'a>(
        &'a self,
        provider_id: &'a str,
        f: Box<CredentialModifyFn<'a>>,
    ) -> BoxFuture<'a, Result<Option<Credential>, StoreError>> {
        Box::pin(async move {
            let lock = self.provider_lock(provider_id).await;
            let _guard = lock.lock().await;

            let current = {
                let credentials = self.inner.credentials.lock().await;
                credentials.get(provider_id).cloned()
            };

            let next = f(current.clone()).await?;
            match next {
                Some(credential) => {
                    let mut credentials = self.inner.credentials.lock().await;
                    credentials.insert(provider_id.to_owned(), credential.clone());
                    Ok(Some(credential))
                }
                // None means no-op: leave the entry unchanged and never delete.
                None => Ok(current),
            }
        })
    }

    fn delete<'a>(&'a self, provider_id: &'a str) -> BoxFuture<'a, Result<(), StoreError>> {
        Box::pin(async move {
            let lock = self.provider_lock(provider_id).await;
            let _guard = lock.lock().await;
            let mut credentials = self.inner.credentials.lock().await;
            credentials.remove(provider_id);
            Ok(())
        })
    }
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::time::Duration;

    use serde_json::json;

    use super::*;
    use crate::auth::types::{ApiKeyCredential, CredentialKind, OAuthCredential};

    fn api_key(key: &str) -> Credential {
        Credential::ApiKey(ApiKeyCredential {
            key: Some(key.into()),
            env: None,
        })
    }

    #[tokio::test]
    async fn modify_none_is_noop_and_never_deletes() -> Result<(), StoreError> {
        let store = InMemoryCredentialStore::new();
        let written = store
            .modify(
                "openai",
                Box::new(|current| {
                    Box::pin(async move {
                        assert!(current.is_none());
                        Ok(Some(api_key("sk-live")))
                    })
                }),
            )
            .await?;
        assert_eq!(written, Some(api_key("sk-live")));

        let after_none = store
            .modify(
                "openai",
                Box::new(|current| {
                    Box::pin(async move {
                        assert_eq!(current, Some(api_key("sk-live")));
                        Ok(None)
                    })
                }),
            )
            .await?;
        assert_eq!(after_none, Some(api_key("sk-live")));
        assert_eq!(store.read("openai").await?, Some(api_key("sk-live")));

        store.delete("openai").await?;
        assert_eq!(store.read("openai").await?, None);
        Ok(())
    }

    #[tokio::test]
    async fn concurrent_modify_is_serialized_per_provider() -> Result<(), String> {
        let store = InMemoryCredentialStore::new();
        store
            .modify(
                "anthropic",
                Box::new(|_| Box::pin(async { Ok(Some(api_key("start"))) })),
            )
            .await
            .map_err(|err| err.to_string())?;

        let in_callback = Arc::new(AtomicUsize::new(0));
        let max_in_callback = Arc::new(AtomicUsize::new(0));
        let first_entered = Arc::new(tokio::sync::Notify::new());

        let store_a = store.clone();
        let in_callback_a = Arc::clone(&in_callback);
        let max_in_callback_a = Arc::clone(&max_in_callback);
        let first_entered_a = Arc::clone(&first_entered);
        let task_a = tokio::spawn(async move {
            store_a
                .modify(
                    "anthropic",
                    Box::new(move |current| {
                        let in_callback = in_callback_a;
                        let max_in_callback = max_in_callback_a;
                        let first_entered = first_entered_a;
                        Box::pin(async move {
                            let active = in_callback.fetch_add(1, Ordering::SeqCst) + 1;
                            max_in_callback.fetch_max(active, Ordering::SeqCst);
                            first_entered.notify_one();
                            tokio::time::sleep(Duration::from_millis(40)).await;
                            in_callback.fetch_sub(1, Ordering::SeqCst);
                            let prefix = match current {
                                Some(Credential::ApiKey(ApiKeyCredential {
                                    key: Some(key),
                                    ..
                                })) => key,
                                _ => "missing".into(),
                            };
                            Ok(Some(api_key(&format!("{prefix}-a"))))
                        })
                    }),
                )
                .await
        });

        // Wait until A holds the provider lock inside its callback.
        first_entered.notified().await;

        let store_b = store.clone();
        let in_callback_b = Arc::clone(&in_callback);
        let max_in_callback_b = Arc::clone(&max_in_callback);
        let b_started = Arc::new(AtomicUsize::new(0));
        let b_started_flag = Arc::clone(&b_started);
        let task_b = tokio::spawn(async move {
            b_started_flag.store(1, Ordering::SeqCst);
            store_b
                .modify(
                    "anthropic",
                    Box::new(move |current| {
                        let in_callback = in_callback_b;
                        let max_in_callback = max_in_callback_b;
                        Box::pin(async move {
                            let active = in_callback.fetch_add(1, Ordering::SeqCst) + 1;
                            max_in_callback.fetch_max(active, Ordering::SeqCst);
                            tokio::time::sleep(Duration::from_millis(10)).await;
                            in_callback.fetch_sub(1, Ordering::SeqCst);
                            let prefix = match current {
                                Some(Credential::ApiKey(ApiKeyCredential {
                                    key: Some(key),
                                    ..
                                })) => key,
                                _ => "missing".into(),
                            };
                            Ok(Some(api_key(&format!("{prefix}-b"))))
                        })
                    }),
                )
                .await
        });

        // B has started modify and must be blocked on the provider lock while A sleeps.
        tokio::time::sleep(Duration::from_millis(10)).await;
        assert_eq!(b_started.load(Ordering::SeqCst), 1);
        assert_eq!(in_callback.load(Ordering::SeqCst), 1);

        let result_a = task_a
            .await
            .map_err(|err| err.to_string())?
            .map_err(|err| err.to_string())?;
        let result_b = task_b
            .await
            .map_err(|err| err.to_string())?
            .map_err(|err| err.to_string())?;
        assert_eq!(result_a, Some(api_key("start-a")));
        assert_eq!(result_b, Some(api_key("start-a-b")));
        assert_eq!(
            store
                .read("anthropic")
                .await
                .map_err(|err| err.to_string())?,
            Some(api_key("start-a-b"))
        );
        assert_eq!(
            max_in_callback.load(Ordering::SeqCst),
            1,
            "callback bodies must not overlap"
        );
        Ok(())
    }

    #[tokio::test]
    async fn list_returns_metadata_without_touching_secret_material() -> Result<(), StoreError> {
        let store = InMemoryCredentialStore::new();
        store
            .modify(
                "openai-codex",
                Box::new(|_| {
                    Box::pin(async {
                        Ok(Some(Credential::Oauth(OAuthCredential {
                            refresh: "refresh".into(),
                            access: "access".into(),
                            expires: 1,
                            extra: BTreeMap::from([("accountId".into(), json!("acct"))]),
                        })))
                    })
                }),
            )
            .await?;
        store
            .modify(
                "openai",
                Box::new(|_| {
                    Box::pin(async {
                        Ok(Some(Credential::ApiKey(ApiKeyCredential {
                            key: Some("!echo should-not-run-on-list".into()),
                            env: None,
                        })))
                    })
                }),
            )
            .await?;

        let mut listed = store.list().await?;
        listed.sort_by(|a, b| a.provider_id.cmp(&b.provider_id));
        assert_eq!(
            listed,
            vec![
                CredentialInfo {
                    provider_id: "openai".into(),
                    kind: CredentialKind::ApiKey,
                },
                CredentialInfo {
                    provider_id: "openai-codex".into(),
                    kind: CredentialKind::Oauth,
                },
            ]
        );
        Ok(())
    }
}
