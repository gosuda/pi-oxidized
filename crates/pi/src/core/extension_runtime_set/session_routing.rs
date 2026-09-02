//! Session routing: owns the bound session target, its mirror publication
//! authority, and the correlated-response routing for bridge events.
//!
//! # Coupling choice (WHY)
//!
//! `SessionRouter` holds shared `Arc` handles to `PublishedRuntimeState`
//! (for route claiming and endpoint leasing) and `ReplacementGate` (for
//! token/owner matching via the existing `pub(super)` seams). Storing these
//! handles rather than passing them per-call is the minimal coupling: the
//! router genuinely needs both resources for every routing decision, the
//! handles are cheap `Arc` clones of state the facade already owns, and each
//! crossing stays named — no new token knowledge enters this module, and
//! route-table access goes through the same `claim_route`/`lease` methods the
//! facade used today.

use std::sync::{Arc, Mutex as StdMutex, Weak};

use futures::stream::{FuturesUnordered, StreamExt};
use serde_json::Value;

use super::replacement::ReplacementGate;
use super::{Endpoint, EndpointId, GenerationLease, PublishedRuntimeState};
use crate::core::agent_session::AgentSession;
use crate::core::agent_session::tree::NavigateTreeResult;
use crate::core::agent_session::{BridgeMethod, BridgeRequestId, ExtensionHostError, SessionState};
use crate::core::extension_host::SessionBridgeEvent;

/// Authority carried by the current committed session target. This tag lets
/// a publisher prove it was bound before a subsequent rebind changed target.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct SessionTargetBinding(u64);

/// Centralized routing result so bridge consumers do not duplicate state
/// matching. Created by `SessionRouter::route_session_bridge`.
pub(crate) enum SessionBridgeRoute {
    /// Route to the committed session and retain its publication authority.
    Active {
        target: Arc<AgentSession>,
        binding: SessionTargetBinding,
    },
    /// Route to the exact token-and-owner candidate without publishing its mirror.
    Candidate(Arc<AgentSession>),
    /// Apply an exact token-and-owner readiness operation.
    Operation,
    /// Drop or answer an event that does not carry current authority.
    Rejected,
}

#[derive(Clone, Copy, Debug)]
enum TaggedBridgeKind {
    Candidate,
    Operation,
}

enum BridgeScope<'a> {
    Untagged,
    Tagged {
        token: &'a str,
        origin: Option<EndpointId>,
        kind: TaggedBridgeKind,
    },
}

/// State that owns the bound session target and its mirror publication authority.
struct SessionTargetState {
    target: Weak<AgentSession>,
    binding: Option<SessionTargetBinding>,
    published: bool,
    next_binding: u64,
}

impl SessionTargetState {
    fn new() -> Self {
        Self {
            target: Weak::new(),
            binding: None,
            published: false,
            next_binding: 1,
        }
    }

    fn bind(&mut self, target: Weak<AgentSession>) -> SessionTargetBinding {
        if self.target.ptr_eq(&target)
            && let Some(binding) = self.binding
        {
            return binding;
        }
        let binding = SessionTargetBinding(self.next_binding);
        self.next_binding = self.next_binding.wrapping_add(1).max(1);
        self.target = target;
        self.binding = Some(binding);
        self.published = false;
        binding
    }

    fn route(&self) -> Option<(Arc<AgentSession>, SessionTargetBinding)> {
        Some((self.target.upgrade()?, self.binding?))
    }

    fn binding_for(&self, target: &Arc<AgentSession>) -> Option<SessionTargetBinding> {
        let (current, binding) = self.route()?;
        (self.published && Arc::ptr_eq(&current, target)).then_some(binding)
    }

    fn is_current(&self, binding: SessionTargetBinding) -> bool {
        self.binding == Some(binding) && self.target.strong_count() != 0
    }

    fn is_published(&self, binding: SessionTargetBinding) -> bool {
        self.published && self.is_current(binding)
    }

    fn publish(&mut self, binding: SessionTargetBinding) -> bool {
        if !self.is_current(binding) {
            return false;
        }
        self.published = true;
        true
    }
}

/// Owns the session target state, the publication serialization lock, and the
/// correlated-response routing for all session bridge events.
pub(super) struct SessionRouter {
    target: StdMutex<SessionTargetState>,
    publish_lock: tokio::sync::Mutex<()>,
    state: Arc<StdMutex<PublishedRuntimeState>>,
    replacement: Arc<ReplacementGate>,
}

impl SessionRouter {
    pub(super) fn new(
        state: Arc<StdMutex<PublishedRuntimeState>>,
        replacement: Arc<ReplacementGate>,
    ) -> Self {
        Self {
            target: StdMutex::new(SessionTargetState::new()),
            publish_lock: tokio::sync::Mutex::new(()),
            state,
            replacement,
        }
    }

    fn state(&self) -> std::sync::MutexGuard<'_, PublishedRuntimeState> {
        self.state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    fn lease(&self) -> GenerationLease {
        self.state().lease()
    }

    fn claim_route_and_bind_owner(
        &self,
        id: BridgeRequestId,
        bind_token: Option<&str>,
    ) -> Option<(GenerationLease, Endpoint, BridgeRequestId)> {
        // Keep route claim and owner binding atomic against endpoint retirement.
        let mut state = self.state();
        let claimed = state.claim_route(id)?;
        if let Some(bind_token) = bind_token {
            self.replacement.bind_owner(bind_token, claimed.1.id);
        }
        Some(claimed)
    }

    /// Bind the session that receives facade commands after ready finalization.
    pub(super) async fn bind_session_target(
        &self,
        session: Weak<AgentSession>,
    ) -> SessionTargetBinding {
        let _publish = self.publish_lock.lock().await;
        self.target
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .bind(session)
    }

    /// Return the binding only when `session` owns the published mirror.
    #[must_use]
    pub(super) fn session_binding_for(
        &self,
        session: &Arc<AgentSession>,
    ) -> Option<SessionTargetBinding> {
        self.target
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .binding_for(session)
    }

    /// Commit an already-bound replacement and release its exact token.
    pub(super) async fn commit_session_replacement(
        &self,
        token: &str,
    ) -> Option<(Arc<AgentSession>, SessionTargetBinding)> {
        let _publish = self.publish_lock.lock().await;
        let (target, binding) = self
            .target
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .route()?;
        if !self.replacement.finalize_commit_target(token, &target) {
            return None;
        }
        Some((target, binding))
    }

    /// Whether a mirror publisher still owns the committed session binding.
    #[must_use]
    pub(super) fn is_session_target_current(&self, binding: SessionTargetBinding) -> bool {
        self.target
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .is_current(binding)
    }

    /// Route one bridge item from the sole token, owner, and session authority.
    #[must_use]
    pub(super) fn route_session_bridge(&self, event: &SessionBridgeEvent) -> SessionBridgeRoute {
        let scope = match event {
            SessionBridgeEvent::Command { envelope, origin } => {
                match envelope.replacement_token.as_deref() {
                    Some(token) => BridgeScope::Tagged {
                        token,
                        origin: *origin,
                        kind: TaggedBridgeKind::Candidate,
                    },
                    None => BridgeScope::Untagged,
                }
            }
            SessionBridgeEvent::SetupEntries {
                request, origin, ..
            } => BridgeScope::Tagged {
                token: &request.replacement_token,
                origin: *origin,
                kind: TaggedBridgeKind::Candidate,
            },
            SessionBridgeEvent::ReplacementReady { token, origin }
            | SessionBridgeEvent::ReplacementAbort { token, origin } => BridgeScope::Tagged {
                token,
                origin: *origin,
                kind: TaggedBridgeKind::Operation,
            },
            SessionBridgeEvent::SetModel { .. }
            | SessionBridgeEvent::Compact { .. }
            | SessionBridgeEvent::NewSession { .. }
            | SessionBridgeEvent::Fork { .. }
            | SessionBridgeEvent::NavigateTree { .. }
            | SessionBridgeEvent::SwitchSession { .. }
            | SessionBridgeEvent::Reload { .. } => BridgeScope::Untagged,
        };

        if let BridgeScope::Tagged {
            token,
            origin,
            kind,
        } = scope
        {
            // Token matching belongs to the replacement gate; the router
            // only maps its decision onto a route.
            return match kind {
                TaggedBridgeKind::Candidate => self
                    .replacement
                    .candidate_target_for(token, origin)
                    .map_or(SessionBridgeRoute::Rejected, SessionBridgeRoute::Candidate),
                TaggedBridgeKind::Operation => {
                    if self.replacement.operation_accepts(token, origin) {
                        SessionBridgeRoute::Operation
                    } else {
                        SessionBridgeRoute::Rejected
                    }
                }
            };
        }

        let target = self
            .target
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .route();
        match target {
            Some((target, binding)) => SessionBridgeRoute::Active { target, binding },
            None => SessionBridgeRoute::Rejected,
        }
    }

    /// Route a correlated model response to its originating endpoint.
    ///
    /// # Errors
    ///
    /// Returns an error when the response route is stale or missing, or its host rejects it.
    pub(super) async fn respond_set_model(
        &self,
        id: BridgeRequestId,
        success: bool,
    ) -> Result<(), ExtensionHostError> {
        let Some((_lease, endpoint, local)) = self.state().claim_route(id) else {
            return Err(ExtensionHostError::NotRunning);
        };
        endpoint.runner.respond_set_model(local, success).await
    }

    /// Route a correlated compaction response to its originating endpoint.
    ///
    /// # Errors
    ///
    /// Returns an error when the response route is stale or missing, or its host rejects it.
    pub(super) async fn respond_compact(
        &self,
        id: BridgeRequestId,
        outcome: Result<Value, String>,
    ) -> Result<(), ExtensionHostError> {
        let Some((_lease, endpoint, local)) = self.state().claim_route(id) else {
            return Err(ExtensionHostError::NotRunning);
        };
        endpoint.runner.respond_compact(local, outcome).await
    }

    /// Route a correlated new-session response to its originating endpoint.
    ///
    /// # Errors
    ///
    /// Returns an error when the response route is stale or missing, or its host rejects it.
    pub(super) async fn respond_new_session(
        &self,
        id: BridgeRequestId,
        cancelled: bool,
        token: Option<&str>,
    ) -> Result<(), ExtensionHostError> {
        let bind_token = (!cancelled).then_some(token).flatten();
        let Some((_lease, endpoint, local)) = self.claim_route_and_bind_owner(id, bind_token)
        else {
            return Err(ExtensionHostError::NotRunning);
        };
        endpoint
            .runner
            .respond_new_session(local, cancelled, token)
            .await
    }

    /// Route a correlated fork response to its originating endpoint.
    ///
    /// # Errors
    ///
    /// Returns an error when the response route is stale or missing, or its host rejects it.
    pub(super) async fn respond_fork(
        &self,
        id: BridgeRequestId,
        cancelled: bool,
        selected_text: Option<&str>,
        token: Option<&str>,
    ) -> Result<(), ExtensionHostError> {
        let bind_token = (!cancelled).then_some(token).flatten();
        let Some((_lease, endpoint, local)) = self.claim_route_and_bind_owner(id, bind_token)
        else {
            return Err(ExtensionHostError::NotRunning);
        };
        endpoint
            .runner
            .respond_fork(local, cancelled, selected_text, token)
            .await
    }

    /// Route a correlated tree-navigation response to its originating endpoint.
    ///
    /// # Errors
    ///
    /// Returns an error when the response route is stale or missing, or its host rejects it.
    pub(super) async fn respond_navigate_tree(
        &self,
        id: BridgeRequestId,
        outcome: Result<NavigateTreeResult, String>,
    ) -> Result<(), ExtensionHostError> {
        let Some((_lease, endpoint, local)) = self.state().claim_route(id) else {
            return Err(ExtensionHostError::NotRunning);
        };
        endpoint.runner.respond_navigate_tree(local, outcome).await
    }

    /// Route a correlated switch-session response to its originating endpoint.
    ///
    /// # Errors
    ///
    /// Returns an error when the response route is stale or missing, or its host rejects it.
    pub(super) async fn respond_switch_session(
        &self,
        id: BridgeRequestId,
        cancelled: bool,
        token: Option<&str>,
    ) -> Result<(), ExtensionHostError> {
        let bind_token = (!cancelled).then_some(token).flatten();
        let Some((_lease, endpoint, local)) = self.claim_route_and_bind_owner(id, bind_token)
        else {
            return Err(ExtensionHostError::NotRunning);
        };
        endpoint
            .runner
            .respond_switch_session(local, cancelled, token)
            .await
    }

    /// Route a correlated reload response to its originating endpoint.
    ///
    /// # Errors
    ///
    /// Returns an error when the response route is stale or missing, or its host rejects it.
    pub(super) async fn respond_reload(
        &self,
        id: BridgeRequestId,
        outcome: Result<Option<&str>, String>,
    ) -> Result<(), ExtensionHostError> {
        let bind_token = outcome.as_ref().ok().and_then(|token| *token);
        let Some((_lease, endpoint, local)) = self.claim_route_and_bind_owner(id, bind_token)
        else {
            return Err(ExtensionHostError::NotRunning);
        };
        endpoint.runner.respond_reload(local, outcome).await
    }

    /// Route a correlated setup-entries response to its originating endpoint.
    ///
    /// # Errors
    ///
    /// Returns an error when the response route is stale or missing, or its host rejects it.
    pub(super) async fn respond_setup_entries(
        &self,
        id: BridgeRequestId,
        outcome: Result<Vec<Value>, String>,
    ) -> Result<(), ExtensionHostError> {
        let Some((_lease, endpoint, local)) = self.state().claim_route(id) else {
            return Err(ExtensionHostError::NotRunning);
        };
        endpoint.runner.respond_setup_entries(local, outcome).await
    }

    /// Validate a replacement token and return the pending replacement target
    /// session. Returns `None` for stale or missing tokens (fail closed).
    #[must_use]
    pub(super) fn validate_setup_token(&self, token: &str) -> Option<Arc<AgentSession>> {
        self.replacement.setup_target_for(token)
    }

    /// Route a deterministic busy rejection to the request's originating endpoint.
    ///
    /// # Errors
    ///
    /// Returns an error when the response route is stale or its host rejects it.
    pub(super) async fn respond_replacement_busy(
        &self,
        id: BridgeRequestId,
        method: BridgeMethod,
    ) -> Result<(), ExtensionHostError> {
        let Some((_lease, endpoint, local)) = self.state().claim_route(id) else {
            return Err(ExtensionHostError::NotRunning);
        };
        endpoint
            .runner
            .respond_replacement_busy(local, method)
            .await
    }

    /// Route an `extension_error` response to the request's originating endpoint.
    ///
    /// # Errors
    ///
    /// Returns an error when the response route is stale or its host rejects it.
    pub(super) async fn respond_session_error(
        &self,
        id: BridgeRequestId,
        method: BridgeMethod,
        message: &str,
    ) -> Result<(), ExtensionHostError> {
        let Some((_lease, endpoint, local)) = self.state().claim_route(id) else {
            return Err(ExtensionHostError::NotRunning);
        };
        endpoint
            .runner
            .respond_session_error(local, method, message)
            .await
    }

    /// Publish the first mirror snapshot and grant this binding publication authority.
    pub(super) async fn activate_session_state(
        &self,
        binding: SessionTargetBinding,
        state: &SessionState,
    ) -> bool {
        let _publish = self.publish_lock.lock().await;
        if !self
            .target
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .is_current(binding)
        {
            return false;
        }
        self.broadcast_session_state(state).await;
        self.target
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .publish(binding)
    }

    /// Broadcast mirrored session state only for the published binding.
    pub(super) async fn push_session_state_for_binding(
        &self,
        binding: SessionTargetBinding,
        state: &SessionState,
    ) -> bool {
        let _publish = self.publish_lock.lock().await;
        if !self
            .target
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .is_published(binding)
        {
            return false;
        }
        self.broadcast_session_state(state).await;
        true
    }

    async fn broadcast_session_state(&self, state: &SessionState) {
        let lease = self.lease();
        let mut sends = FuturesUnordered::new();
        for endpoint in lease.live_endpoints() {
            sends.push(endpoint.runner.push_session_state(state));
        }
        while sends.next().await.is_some() {}
    }
}
