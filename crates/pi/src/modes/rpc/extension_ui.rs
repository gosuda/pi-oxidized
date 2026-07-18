//! Extension UI proxy for RPC mode.
//!
//! Ports the interactive-dialog correlation layer from
//! `.references/pi/packages/coding-agent/src/modes/rpc/rpc-mode.ts`
//! (`createDialogPromise`, `createExtensionUIContext`).
//!
//! In RPC mode the agent runs headless and all extension UI interactions are
//! proxied through the JSONL protocol:
//!
//! 1. An extension (via the future TypeScript host) calls a UI method such as
//!    `select` / `confirm` / `input` / `editor`.
//! 2. [`ExtensionUiProxy::create_dialog`] registers a pending entry keyed by a
//!    fresh UUID and returns the [`RpcExtensionUiRequest`] to emit plus a
//!    [`oneshot::Receiver`] to await.
//! 3. The server writes the request to stdout and awaits the receiver (with an
//!    optional timeout / cancellation token). On timeout or cancellation the
//!    default value is resolved — `None` for select/input/editor, `false` for
//!    confirm.
//! 4. The RPC client sends back an [`RpcExtensionUiResponse`] on stdin.
//! 5. [`ExtensionUiProxy::route_response`] resolves the pending promise.
//!    Orphan responses (no matching pending id) are silently dropped.
//!
//! Fire-and-forget methods (`notify`, `setStatus`, `setWidget`, `setTitle`,
//! `setEditorText`) emit a request with a fresh id but never await a response.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use tokio::sync::oneshot;
use uuid::Uuid;

use super::types::{NotifyType, RpcExtensionUiRequest, RpcExtensionUiResponse, WidgetPlacement};

// ---------------------------------------------------------------------------
// Pending request map
// ---------------------------------------------------------------------------

type Resolver = oneshot::Sender<RpcExtensionUiResponse>;

fn lock_pending(
    pending: &Mutex<HashMap<String, Resolver>>,
) -> std::sync::MutexGuard<'_, HashMap<String, Resolver>> {
    pending
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

// ---------------------------------------------------------------------------
// ExtensionUiProxy
// ---------------------------------------------------------------------------

/// Manages pending extension UI requests and provides helpers for creating
/// dialog promises and constructing fire-and-forget request frames.
///
/// Cloneable and thread-safe: the pending map lives behind an
/// [`Arc`]`<`[`Mutex`]`>`. Multiple tasks (the stdin reader, extension hooks)
/// can create dialogs and route responses concurrently.
#[derive(Clone)]
pub struct ExtensionUiProxy {
    pending: Arc<Mutex<HashMap<String, Resolver>>>,
}

impl ExtensionUiProxy {
    /// Create an empty proxy.
    #[must_use]
    pub fn new() -> Self {
        Self {
            pending: Arc::new(Mutex::new(HashMap::new())),
        }
    }

    /// Generate a fresh UUID correlation id.
    #[must_use]
    fn fresh_id() -> String {
        Uuid::new_v4().to_string()
    }

    /// Register a pending dialog and return the request to emit plus the
    /// receiver to await.
    ///
    /// The caller:
    /// 1. Serializes and writes `request` to stdout.
    /// 2. Awaits `receiver` (optionally with `tokio::time::timeout`).
    /// 3. On timeout or [`RecvError`], resolves with the dialog default.
    #[must_use]
    pub fn create_dialog(
        &self,
        build: impl FnOnce(&str) -> RpcExtensionUiRequest,
    ) -> (
        RpcExtensionUiRequest,
        oneshot::Receiver<RpcExtensionUiResponse>,
    ) {
        let id = Self::fresh_id();
        let request = build(&id);
        let (tx, rx) = oneshot::channel();
        lock_pending(&self.pending).insert(id, tx);
        (request, rx)
    }

    /// Route an incoming [`RpcExtensionUiResponse`] to its pending dialog.
    ///
    /// Returns `true` when a matching pending request was found and resolved.
    /// Orphan responses (no matching pending id) are silently dropped (the TS
    /// reference does the same: `const pending = …; if (pending) { … }`).
    #[must_use]
    pub fn route_response(&self, response: RpcExtensionUiResponse) -> bool {
        let id = response.id().to_owned();
        if let Some(tx) = lock_pending(&self.pending).remove(&id) {
            // Sending can only fail if the receiver was already dropped
            // (timeout/cancel resolved first). That's benign.
            let _ = tx.send(response);
            true
        } else {
            false
        }
    }

    /// Drop all pending requests. Each receiver will observe [`RecvError`]
    /// and resolve with its default value. Called on shutdown.
    pub fn cancel_all(&self) {
        lock_pending(&self.pending).clear();
    }

    /// Number of currently pending requests (test observable).
    #[must_use]
    pub fn pending_count(&self) -> usize {
        lock_pending(&self.pending).len()
    }

    // -----------------------------------------------------------------------
    // Fire-and-forget request constructors
    //
    // These return an [`RpcExtensionUiRequest`] with a fresh id. The caller
    // serializes and writes the frame to stdout; no response is awaited.
    // -----------------------------------------------------------------------

    /// Build a `notify` request (fire-and-forget).
    #[must_use]
    pub fn notify(message: &str, notify_type: Option<NotifyType>) -> RpcExtensionUiRequest {
        RpcExtensionUiRequest::Notify {
            id: Self::fresh_id(),
            message: message.to_owned(),
            notify_type,
        }
    }

    /// Build a `setStatus` request (fire-and-forget). `text = None` clears.
    #[must_use]
    pub fn set_status(key: &str, text: Option<&str>) -> RpcExtensionUiRequest {
        RpcExtensionUiRequest::SetStatus {
            id: Self::fresh_id(),
            status_key: key.to_owned(),
            status_text: text.map(str::to_owned),
        }
    }

    /// Build a `setWidget` request (fire-and-forget).
    /// `lines = None` clears the widget.
    #[must_use]
    pub fn set_widget(
        key: &str,
        lines: Option<&[String]>,
        placement: Option<WidgetPlacement>,
    ) -> RpcExtensionUiRequest {
        RpcExtensionUiRequest::SetWidget {
            id: Self::fresh_id(),
            widget_key: key.to_owned(),
            widget_lines: lines.map(<[String]>::to_vec),
            widget_placement: placement,
        }
    }

    /// Build a `setTitle` request (fire-and-forget).
    #[must_use]
    pub fn set_title(title: &str) -> RpcExtensionUiRequest {
        RpcExtensionUiRequest::SetTitle {
            id: Self::fresh_id(),
            title: title.to_owned(),
        }
    }

    /// Build a `set_editor_text` request (fire-and-forget).
    #[must_use]
    pub fn set_editor_text(text: &str) -> RpcExtensionUiRequest {
        RpcExtensionUiRequest::SetEditorText {
            id: Self::fresh_id(),
            text: text.to_owned(),
        }
    }
}

impl Default for ExtensionUiProxy {
    fn default() -> Self {
        Self::new()
    }
}

impl std::fmt::Debug for ExtensionUiProxy {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ExtensionUiProxy")
            .field("pending", &self.pending_count())
            .finish()
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::panic, clippy::expect_used)]
mod tests {
    use super::*;
    use crate::modes::rpc::types::RpcExtensionUiResponse;

    #[tokio::test]
    async fn create_dialog_and_route_response() {
        let proxy = ExtensionUiProxy::new();
        let (req, rx) = proxy.create_dialog(|id| RpcExtensionUiRequest::Select {
            id: id.to_owned(),
            title: "Pick".into(),
            options: vec!["a".into(), "b".into()],
            timeout: None,
        });
        let id = req.id().to_owned();
        assert_eq!(proxy.pending_count(), 1);

        let resolved = proxy.route_response(RpcExtensionUiResponse::Value {
            id,
            value: "b".into(),
        });
        assert!(resolved);
        assert_eq!(proxy.pending_count(), 0);

        let resp = rx.await.unwrap();
        match resp {
            RpcExtensionUiResponse::Value { value, .. } => assert_eq!(value, "b"),
            _ => panic!("expected Value"),
        }
    }

    #[tokio::test]
    async fn orphan_response_silently_dropped() {
        let proxy = ExtensionUiProxy::new();
        let resolved = proxy.route_response(RpcExtensionUiResponse::Cancelled {
            id: "nonexistent".into(),
        });
        assert!(!resolved);
        assert_eq!(proxy.pending_count(), 0);
    }

    #[tokio::test]
    async fn cancel_all_drops_pending() {
        let proxy = ExtensionUiProxy::new();
        let (_req, rx) = proxy.create_dialog(|id| RpcExtensionUiRequest::Confirm {
            id: id.to_owned(),
            title: "Sure?".into(),
            message: "Continue?".into(),
            timeout: None,
        });
        assert_eq!(proxy.pending_count(), 1);

        proxy.cancel_all();
        assert_eq!(proxy.pending_count(), 0);

        // Receiver observes the error → caller resolves with default.
        assert!(rx.await.is_err());
    }

    #[tokio::test]
    async fn route_after_cancel_is_orphan() {
        let proxy = ExtensionUiProxy::new();
        let (req, _rx) = proxy.create_dialog(|id| RpcExtensionUiRequest::Input {
            id: id.to_owned(),
            title: "Name".into(),
            placeholder: None,
            timeout: None,
        });
        let id = req.id().to_owned();

        proxy.cancel_all();
        let resolved = proxy.route_response(RpcExtensionUiResponse::Value {
            id,
            value: "x".into(),
        });
        assert!(!resolved);
    }

    #[test]
    fn fire_and_forget_constructors_have_unique_ids() {
        let n1 = ExtensionUiProxy::notify("hi", None);
        let n2 = ExtensionUiProxy::notify("hi", None);
        assert_ne!(n1.id(), n2.id());

        let s1 = ExtensionUiProxy::set_status("key", Some("val"));
        let s2 = ExtensionUiProxy::set_status("key", None);
        assert_ne!(s1.id(), s2.id());

        let w = ExtensionUiProxy::set_widget("w", Some(&["a".into()]), None);
        assert!(!w.id().is_empty());

        let t = ExtensionUiProxy::set_title("Title");
        assert!(!t.id().is_empty());

        let e = ExtensionUiProxy::set_editor_text("text");
        assert!(!e.id().is_empty());
    }

    #[test]
    fn clone_shares_pending_map() {
        let proxy = ExtensionUiProxy::new();
        let cloned = proxy.clone();
        let (_req, _rx) = cloned.create_dialog(|id| RpcExtensionUiRequest::Editor {
            id: id.to_owned(),
            title: "T".into(),
            prefill: None,
        });
        // Original sees the pending entry created via the clone.
        assert_eq!(proxy.pending_count(), 1);
    }

    #[tokio::test]
    async fn concurrent_create_and_route() {
        let proxy = Arc::new(ExtensionUiProxy::new());
        let mut handles = Vec::new();

        for i in 0..8 {
            let p = Arc::clone(&proxy);
            handles.push(tokio::spawn(async move {
                let (req, rx) = p.create_dialog(|id| RpcExtensionUiRequest::Select {
                    id: id.to_owned(),
                    title: format!("Pick {i}"),
                    options: vec!["x".into()],
                    timeout: None,
                });
                let id = req.id().to_owned();
                // Simulate client response.
                let p2 = Arc::clone(&p);
                tokio::spawn(async move {
                    let _ = p2.route_response(RpcExtensionUiResponse::Value {
                        id,
                        value: "x".into(),
                    });
                });
                rx.await.unwrap()
            }));
        }

        for h in handles {
            let resp = h.await.unwrap();
            assert!(matches!(resp, RpcExtensionUiResponse::Value { .. }));
        }
        assert_eq!(proxy.pending_count(), 0);
    }

    #[test]
    fn default_is_empty() {
        let proxy = ExtensionUiProxy::default();
        assert_eq!(proxy.pending_count(), 0);
    }
}
