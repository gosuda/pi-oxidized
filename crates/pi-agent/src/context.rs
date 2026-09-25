//! Typed context and cancellation for the durable session and harness layers.
//!
//! A [`Context`] is a cancellation scope plus a persistent typed value chain. It
//! is cheap to clone (an `Arc` chain with copy-on-write derivation) and
//! derivation never mutates the parent, so a child scope can add values or
//! narrow cancellation without the caller observing the change.
//!
//! Every `session` and `harness` trait method takes `cx: &Context` as its last
//! parameter.

use std::any::{Any, TypeId};
use std::future::Future;
use std::marker::PhantomData;
use std::sync::Arc;

use tokio_util::sync::CancellationToken;

use crate::telemetry::{TelemetryContext, noop_context};

/// Typed, cloneable key for one context value slot.
///
/// Slot identity is the pair `(TypeId::of::<T>(), name)`, so two keys with the
/// same payload type but different names address different slots.
pub struct ContextKey<T> {
    name: &'static str,
    _marker: PhantomData<fn() -> T>,
}

impl<T: Send + Sync + 'static> ContextKey<T> {
    /// Creates a key for the slot named `name`.
    #[must_use]
    pub const fn new(name: &'static str) -> Self {
        Self {
            name,
            _marker: PhantomData,
        }
    }

    /// Returns the slot name.
    #[must_use]
    pub const fn name(&self) -> &'static str {
        self.name
    }
}

impl<T> Clone for ContextKey<T> {
    fn clone(&self) -> Self {
        *self
    }
}

impl<T> Copy for ContextKey<T> {}

impl<T> std::fmt::Debug for ContextKey<T> {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("ContextKey")
            .field("name", &self.name)
            .finish()
    }
}

/// One link in the persistent value chain.
struct ValueNode {
    key: TypeId,
    name: &'static str,
    value: Arc<dyn Any + Send + Sync>,
    parent: Option<Arc<ValueNode>>,
}

/// Cancellation scope plus a persistent typed value chain.
#[derive(Clone, Default)]
pub struct Context {
    token: Option<CancellationToken>,
    values: Option<Arc<ValueNode>>,
}

impl Context {
    /// A context that is never cancelled and carries no values.
    #[must_use]
    pub fn background() -> Self {
        Self {
            token: None,
            values: None,
        }
    }
    /// Explicit placeholder retained for source compatibility with the frozen
    /// Chord API. Durable code should receive a real caller context.
    #[must_use]
    pub fn todo() -> Self {
        Self::background()
    }
    /// Derives a context carrying `value` at `key`.
    #[must_use]
    pub fn with_value<T: Send + Sync + 'static>(&self, key: ContextKey<T>, value: T) -> Self {
        Self {
            token: self.token.clone(),
            values: Some(Arc::new(ValueNode {
                key: TypeId::of::<T>(),
                name: key.name(),
                value: Arc::new(value),
                parent: self.values.clone(),
            })),
        }
    }

    /// Reads the most recently written value at `key`.
    #[must_use]
    pub fn value<T: Send + Sync + 'static>(&self, key: ContextKey<T>) -> Option<Arc<T>> {
        let wanted = TypeId::of::<T>();
        let mut cursor = self.values.as_ref();
        while let Some(node) = cursor {
            if node.key == wanted && node.name == key.name() {
                return Arc::clone(&node.value).downcast::<T>().ok();
            }
            cursor = node.parent.as_ref();
        }
        None
    }

    /// Derives a context cancelled by `token`, replacing any existing token.
    #[must_use]
    pub fn with_cancellation(&self, token: CancellationToken) -> Self {
        Self {
            token: Some(token),
            values: self.values.clone(),
        }
    }

    /// Derives a child scope plus the token that cancels it.
    ///
    /// Cancelling `self` cancels the returned token; cancelling the returned
    /// token leaves `self` untouched.
    #[must_use]
    pub fn with_cancel(&self) -> (Self, CancellationToken) {
        let token = match self.token.as_ref() {
            Some(parent) => parent.child_token(),
            None => CancellationToken::new(),
        };
        (self.with_cancellation(token.clone()), token)
    }

    /// Strips cancellation while retaining every value.
    ///
    /// Required when installing a drive: a drive outlives its caller's
    /// cancellation scope.
    #[must_use]
    pub fn without_cancellation(&self) -> Self {
        Self {
            token: None,
            values: self.values.clone(),
        }
    }

    /// Returns the cancellation token, when this scope has one.
    #[must_use]
    pub fn token(&self) -> Option<&CancellationToken> {
        self.token.as_ref()
    }

    /// Returns `true` once this scope is cancelled.
    #[must_use]
    pub fn is_cancelled(&self) -> bool {
        self.token
            .as_ref()
            .is_some_and(CancellationToken::is_cancelled)
    }

    /// The uniform pre-effect check.
    ///
    /// # Errors
    ///
    /// Returns [`Cancelled`] when this scope is already cancelled.
    pub fn check(&self) -> Result<(), Cancelled> {
        if self.is_cancelled() {
            return Err(Cancelled);
        }
        Ok(())
    }

    /// Races `future` against cancellation.
    ///
    /// # Errors
    ///
    /// Returns [`Cancelled`] when this scope is cancelled before `future`
    /// resolves. An already-cancelled scope never polls `future`.
    pub async fn race<F: Future>(&self, future: F) -> Result<F::Output, Cancelled> {
        let Some(token) = self.token.clone() else {
            return Ok(future.await);
        };
        if token.is_cancelled() {
            return Err(Cancelled);
        }
        tokio::select! {
            biased;
            () = token.cancelled() => Err(Cancelled),
            output = future => Ok(output),
        }
    }
}

impl std::fmt::Debug for Context {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let mut names = Vec::new();
        let mut cursor = self.values.as_ref();
        while let Some(node) = cursor {
            names.push(node.name);
            cursor = node.parent.as_ref();
        }
        formatter
            .debug_struct("Context")
            .field("cancellable", &self.token.is_some())
            .field("cancelled", &self.is_cancelled())
            .field("values", &names)
            .finish()
    }
}

/// The active context was cancelled.
#[derive(Clone, Copy, Debug, thiserror::Error, PartialEq, Eq)]
#[error("context cancelled")]
pub struct Cancelled;

/// Telemetry parent read by harness spans.
pub const TELEMETRY_CONTEXT: ContextKey<Arc<dyn TelemetryContext>> =
    ContextKey::new("pi.telemetryContext");

/// Returns the telemetry parent carried by `cx`, or a no-op context.
#[must_use]
pub fn telemetry_context(cx: &Context) -> Arc<dyn TelemetryContext> {
    cx.value(TELEMETRY_CONTEXT)
        .map_or_else(noop_context, |stored| Arc::clone(&stored))
}

/// Derives a context whose telemetry parent is `telemetry`.
#[must_use]
pub fn with_telemetry_context(telemetry: Arc<dyn TelemetryContext>, cx: &Context) -> Context {
    cx.with_value(TELEMETRY_CONTEXT, telemetry)
}

#[cfg(test)]
mod tests {
    use super::*;

    type TestResult = Result<(), Box<dyn std::error::Error>>;

    const FIRST: ContextKey<u32> = ContextKey::new("first");
    const SECOND: ContextKey<u32> = ContextKey::new("second");

    #[test]
    fn same_type_distinct_names_are_distinct_slots() {
        let cx = Context::background()
            .with_value(FIRST, 1)
            .with_value(SECOND, 2);
        assert_eq!(cx.value(FIRST).as_deref(), Some(&1));
        assert_eq!(cx.value(SECOND).as_deref(), Some(&2));
    }

    #[test]
    fn later_write_shadows_earlier_and_parent_is_unchanged() {
        let parent = Context::background().with_value(FIRST, 1);
        let child = parent.with_value(FIRST, 9);
        assert_eq!(child.value(FIRST).as_deref(), Some(&9));
        assert_eq!(parent.value(FIRST).as_deref(), Some(&1));
    }

    #[test]
    fn missing_slot_reads_none() {
        assert!(Context::background().value(FIRST).is_none());
    }

    #[test]
    fn child_cancels_with_parent_but_not_the_reverse() {
        let root = CancellationToken::new();
        let parent = Context::background().with_cancellation(root.clone());
        let (child, child_token) = parent.with_cancel();

        child_token.cancel();
        assert!(child.is_cancelled());
        assert!(!parent.is_cancelled());

        let (sibling, _sibling_token) = parent.with_cancel();
        root.cancel();
        assert!(sibling.is_cancelled());
        assert!(parent.is_cancelled());
    }

    #[test]
    fn without_cancellation_keeps_values_and_drops_the_token() {
        let token = CancellationToken::new();
        let cx = Context::background()
            .with_value(FIRST, 7)
            .with_cancellation(token.clone());
        token.cancel();

        let detached = cx.without_cancellation();
        assert!(!detached.is_cancelled());
        assert!(detached.check().is_ok());
        assert_eq!(detached.value(FIRST).as_deref(), Some(&7));
    }

    #[test]
    fn check_reports_cancellation() {
        let token = CancellationToken::new();
        let cx = Context::background().with_cancellation(token.clone());
        assert!(cx.check().is_ok());
        token.cancel();
        assert_eq!(cx.check(), Err(Cancelled));
    }

    #[tokio::test]
    async fn race_returns_output_when_not_cancelled() -> TestResult {
        let cx = Context::background().with_cancellation(CancellationToken::new());
        assert_eq!(cx.race(std::future::ready(4)).await?, 4);
        Ok(())
    }

    #[tokio::test]
    async fn race_never_polls_an_already_cancelled_scope() {
        let token = CancellationToken::new();
        token.cancel();
        let cx = Context::background().with_cancellation(token);
        let result = cx.race(std::future::pending::<()>()).await;
        assert_eq!(result, Err(Cancelled));
    }

    #[tokio::test]
    async fn race_loses_to_cancellation() {
        let token = CancellationToken::new();
        let cx = Context::background().with_cancellation(token.clone());
        let cancel = tokio::spawn(async move { token.cancel() });
        let result = cx.race(std::future::pending::<()>()).await;
        assert_eq!(result, Err(Cancelled));
        cancel.await.ok();
    }

    #[tokio::test]
    async fn background_race_has_no_cancellation_path() -> TestResult {
        assert_eq!(Context::background().race(std::future::ready(1)).await?, 1);
        Ok(())
    }

    #[test]
    fn telemetry_context_defaults_to_noop_and_round_trips() {
        let cx = Context::background();
        let default_parent = telemetry_context(&cx);
        let installed = with_telemetry_context(Arc::clone(&default_parent), &cx);
        assert!(Arc::ptr_eq(&telemetry_context(&installed), &default_parent));
    }
}
