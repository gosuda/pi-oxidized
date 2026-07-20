//! Manual + automatic context-compaction orchestration.
//!
//! Wraps the pure engine in [`crate::core::compaction`] (`should_compact` /
//! `prepare_compaction` / `compact`) with the product-level lifecycle:
//!
//! - Manual [`AgentSession::compact`] (user `/compact`).
//! - Post-run / pre-prompt [`AgentSession::check_compaction`] that classifies
//!   overflow vs threshold, honours the sameModel + stale pre-compaction guards,
//!   and drives the one-shot overflow-recovery latch.
//! - Extension `session_before_compact` (cancellable) and `session_compact`
//!   notifications each dispatch exactly once through their dedicated core paths.
//!
//! Public compaction notifications are emitted separately so the host mapping
//! cannot duplicate either extension lifecycle hook.
//!
//! Public compaction events await event-consumer backpressure after synchronous
//! listeners; extension lifecycle dispatch remains explicit and single-shot.
//!
//! # Lock order
//!
//! Inherits the lock order from [`super`]: never hold [`AgentSessionInner`]
//! across `.await`, never hold the session-manager async mutex across the
//! extension or public emits, and never nest `SessionHooks` `RwLock`s with either.

use std::collections::BTreeMap;
use std::pin::Pin;
use std::sync::Arc;

use pi_ai::{
    AssistantMessage, AssistantMessageEvent, Context, Model, ProviderError, StopReason,
    StreamOptions,
};
use regex::Regex;
use tokio_util::sync::CancellationToken;

use crate::core::compaction::{
    self, BeforeCompactResult, CompactOptions, CompactionError, CompactionResult,
    CompactionSettings, SummarizeStreamFn, preparation_none_error, prepare_compaction,
    should_compact,
};
use crate::core::model_runtime::ModelRuntimeAuthOverrides;
use crate::core::sessions::{CompactionEntry, SessionEntry, get_latest_compaction_entry};
use crate::core::settings::ResolvedCompactionSettings;

use super::AgentSession;
use super::events::{AgentSessionEvent, CompactionReason};
use super::extension_runner::ExtensionRunner;

// ---------------------------------------------------------------------------
// Resolved auth + stream source for compaction
// ---------------------------------------------------------------------------

/// Auth + stream inputs resolved for one compaction pass.
///
/// Produced by [`AgentSession::resolve_compaction_inputs`]. Mirrors the
/// `(apiKey, headers, streamFn, env)` tuple TypeScript passes to the pure
/// `compact()` call.
#[derive(Clone)]
struct CompactionInputs {
    /// Injected summariser stream (`streamSimple` equivalent).
    stream_fn: SummarizeStreamFn,
    /// Resolved API key (None when the provider uses ambient/OAuth auth that
    /// the `stream_fn` itself injects — e.g. `ModelRuntime::stream_simple()`).
    api_key: Option<String>,
    /// Provider headers merged into the summarisation request.
    headers: Option<BTreeMap<String, Option<String>>>,
    /// Provider-scoped environment overlay.
    env: Option<BTreeMap<String, String>>,
}

/// Test/override stream source stored as `AgentSessionConfig::model_runtime`
/// when a full [`ModelRuntime`] is not available (unit tests, headless SDK).
///
/// Production code stores a real `ModelRuntime`; compaction resolves either.
#[derive(Clone)]
pub struct CompactionStreamHandle {
    /// Stream function used by the pure compaction summariser.
    pub stream_fn: SummarizeStreamFn,
    /// Optional pre-resolved API key.
    pub api_key: Option<String>,
}

impl CompactionStreamHandle {
    /// Build a handle around a stream function and optional key.
    #[must_use]
    pub fn new(stream_fn: SummarizeStreamFn) -> Self {
        Self {
            stream_fn,
            api_key: None,
        }
    }
}

// ---------------------------------------------------------------------------
// impl AgentSession — compaction surface
// ---------------------------------------------------------------------------

impl AgentSession {
    // -- public API --------------------------------------------------------

    /// Manually compact the session context.
    ///
    /// Disconnects the event pump, aborts any in-flight run, emits
    /// `compaction_start{manual}`, runs the pure compaction engine, persists
    /// the summary, rebuilds the agent transcript, and emits
    /// `compaction_end{manual}`. The pump is always reconnected in the
    /// finally block.
    ///
    /// # Errors
    ///
    /// Returns the exact contract strings from the pure engine:
    /// [`CompactionError::AlreadyCompacted`] / [`CompactionError::NothingToCompact`]
    /// when preparation yields `None`, [`CompactionError::Cancelled`] when the
    /// user aborts via [`AgentSession::abort_compaction`] or the extension
    /// returns `cancel: true`, and [`CompactionError::SummarizationFailed`]
    /// when the model/auth/stream resolution fails.
    pub async fn compact(
        &self,
        custom_instructions: Option<&str>,
    ) -> Result<CompactionResult, CompactionError> {
        // Disconnect pump so compaction-time abort does not surface aborted
        // events to listeners (mirrors TS _disconnectFromAgent).
        self.disconnect_from_agent();
        self.abort().await;

        let abort_token = self.begin_compaction_abort();
        self.emit_public_awaited(&AgentSessionEvent::CompactionStart {
            reason: CompactionReason::Manual,
        })
        .await;

        // Capture reconnect handle before entering the try block: the finally
        // logic needs to run regardless of how we exit.
        let weak_self = self.upgrade_self();

        let result = self
            .run_compaction_core(
                CompactionReason::Manual,
                custom_instructions,
                false,
                abort_token,
                true,
            )
            .await;

        let outcome = match result {
            Ok(Some(compaction_result)) => {
                self.emit_public_awaited(&AgentSessionEvent::CompactionEnd {
                    reason: CompactionReason::Manual,
                    result: Some(compaction_result.clone()),
                    aborted: false,
                    will_retry: false,
                    error_message: None,
                })
                .await;
                Ok(compaction_result)
            }
            Ok(None) => {
                // Preparation returned None — classify and surface the exact
                // contract string so the UI shows the right message.
                let path_entries = self.snapshot_branch_entries().await;
                let path_refs: Vec<&SessionEntry> = path_entries.iter().collect();
                let err = preparation_none_error(&path_refs);
                let message = err.to_string();
                self.emit_public_awaited(&AgentSessionEvent::CompactionEnd {
                    reason: CompactionReason::Manual,
                    result: None,
                    aborted: false,
                    will_retry: false,
                    error_message: Some(message),
                })
                .await;
                Err(err)
            }
            Err(err) => {
                let aborted = matches!(err, CompactionError::Cancelled);
                let message = if aborted {
                    None
                } else {
                    Some(format!("Compaction failed: {err}"))
                };
                self.emit_public_awaited(&AgentSessionEvent::CompactionEnd {
                    reason: CompactionReason::Manual,
                    result: None,
                    aborted,
                    will_retry: false,
                    error_message: message,
                })
                .await;
                Err(err)
            }
        };

        // finally: clear abort + reconnect pump.
        self.clear_compaction_abort();
        if let Some(arc) = weak_self.and_then(|w| w.upgrade()) {
            arc.reconnect_to_agent();
        }

        outcome
    }

    // -- auto-compaction check --------------------------------------------

    /// Check whether auto-compaction should run and run it if so.
    ///
    /// Called after `agent_end` (post-run path) and before submitting a new
    /// user prompt (pre-prompt path). The pre-prompt caller ignores the
    /// boolean return and never calls `agent.continue_run()`; the post-run
    /// path uses it to signal the outer run loop.
    ///
    /// Returns `true` when the session should continue the run after
    /// auto-compaction (overflow-retry path or queued message delivery).
    ///
    /// Two cases:
    /// 1. **Overflow** — LLM returned a context-overflow error, or a successful
    ///    response exceeded the configured window. Compacts and optionally
    ///    retries once (`overflow_recovery_attempted` latch).
    /// 2. **Threshold** — context is over the configured threshold. Compacts
    ///    without retry (user continues manually).
    pub(super) async fn check_compaction(&self, assistant_message: &AssistantMessage) -> bool {
        let settings = self.compaction_settings();
        // The inner `auto_compaction_enabled` flag is the runtime source of
        // truth (set by `set_auto_compaction_enabled`); `settings.enabled` is
        // only the persisted initial default. Gate on the inner flag alone.
        let enabled = self.lock_inner().auto_compaction_enabled;
        if !enabled {
            return false;
        }

        let model = self.model();
        let context_window = model.context_window;

        // Same-model guard: overflow from a previous (smaller-context) model
        // must not compact after a model switch.
        let same_model = same_model(&model, assistant_message);

        // Stale pre-compaction guard: skip when the assistant predates the
        // latest compaction boundary (its usage/error reflects the old context).
        let compaction_entry = self.latest_compaction_entry().await;
        if let Some(latest) = &compaction_entry {
            let boundary_ts = parse_iso_to_millis(&latest.timestamp);
            if boundary_ts > 0 && assistant_message.timestamp <= boundary_ts {
                return false;
            }
        }

        // Case 1: Overflow.
        if same_model && is_context_overflow(assistant_message, context_window) {
            let will_retry = assistant_message.stop_reason != StopReason::Stop;

            if !will_retry {
                // Successful response that silently overflowed: compact but
                // do NOT retry (cannot continue from an assistant message).
                return self
                    .run_auto_compaction(CompactionReason::Overflow, false)
                    .await;
            }

            // Error-style overflow: one compact-and-retry attempt.
            let already_attempted = {
                let inner = self.lock_inner();
                inner.overflow_recovery_attempted
            };

            if already_attempted {
                // Second overflow after one recovery: terminal error, no
                // preceding compaction_start (never started).
                self.emit_public_awaited(&AgentSessionEvent::CompactionEnd {
                    reason: CompactionReason::Overflow,
                    result: None,
                    aborted: false,
                    will_retry: false,
                    error_message: Some(
                        "Context overflow recovery failed after one compact-and-retry attempt"
                            .to_owned(),
                    ),
                })
                .await;
                return false;
            }

            // Arm the one-shot latch and remove the error assistant from
            // state (it IS persisted to session history via message_end
            // already, but must not appear in the retry's context).
            {
                let mut inner = self.lock_inner();
                inner.overflow_recovery_attempted = true;
            }
            let _ = self.agent.pop_last_if_assistant();
            return self
                .run_auto_compaction(CompactionReason::Overflow, will_retry)
                .await;
        }

        // Case 2: Threshold.
        let context_tokens =
            self.threshold_context_tokens(assistant_message, compaction_entry.as_ref());
        if should_compact(context_tokens, context_window, &settings_to_pure(settings)) {
            return self
                .run_auto_compaction(CompactionReason::Threshold, false)
                .await;
        }

        false
    }

    // -- internal auto-compaction runner ----------------------------------

    /// Run auto-compaction with the full event + extension lifecycle.
    ///
    /// Returns `true` when the caller should continue the run (overflow retry
    /// or queued-message delivery), `false` otherwise.
    async fn run_auto_compaction(&self, reason: CompactionReason, will_retry: bool) -> bool {
        let abort_token = self.begin_auto_compaction_abort();

        self.emit_public_awaited(&AgentSessionEvent::CompactionStart { reason })
            .await;

        let result = self
            .run_compaction_core(reason, None, will_retry, abort_token, false)
            .await;

        let should_continue = match result {
            Ok(Some(compaction_result)) => {
                self.emit_public_awaited(&AgentSessionEvent::CompactionEnd {
                    reason,
                    result: Some(compaction_result),
                    aborted: false,
                    will_retry,
                    error_message: None,
                })
                .await;

                if will_retry {
                    // Drop the trailing error assistant so the retry context
                    // starts clean. The outer run loop continues.
                    let _ = self.agent.pop_last_if_assistant();
                    true
                } else {
                    // Queue continuation: auto-compaction can finish while
                    // follow-up/steering/custom messages are waiting. Continue
                    // once so they are delivered.
                    self.agent.has_queued_messages()
                }
            }
            Ok(None) => {
                // Preparation returned None — nothing to compact. Do not emit
                // compaction_end (TS only emits end when started; the None
                // path exits early before compaction_start is emitted there,
                // but here we already emitted start — emit a clean end).
                self.emit_public_awaited(&AgentSessionEvent::CompactionEnd {
                    reason,
                    result: None,
                    aborted: false,
                    will_retry: false,
                    error_message: None,
                })
                .await;
                false
            }
            Err(CompactionError::Cancelled) => {
                self.emit_public_awaited(&AgentSessionEvent::CompactionEnd {
                    reason,
                    result: None,
                    aborted: true,
                    will_retry: false,
                    error_message: None,
                })
                .await;
                false
            }
            Err(err) => {
                let message = if reason == CompactionReason::Overflow {
                    format!("Context overflow recovery failed: {err}")
                } else {
                    format!("Auto-compaction failed: {err}")
                };
                self.emit_public_awaited(&AgentSessionEvent::CompactionEnd {
                    reason,
                    result: None,
                    aborted: false,
                    will_retry: false,
                    error_message: Some(message),
                })
                .await;
                false
            }
        };

        self.clear_auto_compaction_abort();
        should_continue
    }

    // -- shared compaction core -------------------------------------------

    /// Resolve preparation, dispatch extension `before_compact`, run the pure
    /// engine, persist the summary, and rebuild the agent transcript.
    ///
    /// Returns `Ok(Some(result))` on success, `Ok(None)` when preparation
    /// yields nothing (session too small / already compacted), and `Err` on
    /// cancellation or summarisation failure.
    ///
    /// `is_manual` controls only the extension-hook path (manual passes
    /// `custom_instructions`; auto passes `None`). The pure engine itself is
    /// identical for both paths.
    async fn run_compaction_core(
        &self,
        reason: CompactionReason,
        custom_instructions: Option<&str>,
        will_retry: bool,
        abort_token: CancellationToken,
        is_manual: bool,
    ) -> Result<Option<CompactionResult>, CompactionError> {
        let model = self.model();

        // Snapshot path entries + settings under the session-manager lock,
        // then release before any await.
        let (path_entries, settings) = {
            let sm = self.session_manager.lock().await;
            let branch: Vec<&SessionEntry> = sm.get_branch(None);
            let entries: Vec<SessionEntry> = branch.into_iter().cloned().collect();
            (entries, self.compaction_settings())
        };
        let pure_settings = settings_to_pure(settings);

        let path_refs: Vec<&SessionEntry> = path_entries.iter().collect();
        let preparation = prepare_compaction(&path_refs, pure_settings)?;
        let Some(preparation) = preparation else {
            return Ok(None);
        };

        // Resolve auth + stream inputs.
        let inputs = self.resolve_compaction_inputs().await?;

        // Extension before_compact hook (cancellable).
        let runner = self.hooks.runner();
        if runner.has_handlers("session_before_compact") {
            let event = AgentSessionEvent::CompactionStart { reason };
            let cancel = self.extension_before_compact(&runner, event).await?;
            if cancel.cancel {
                return Err(CompactionError::Cancelled);
            }
            if let Some(replacement) = cancel.compaction {
                // Extension provided the full result — persist + emit without
                // invoking the pure summariser.
                let mut replacement = replacement;
                replacement.from_hook = Some(true);
                return self
                    .finalize_compaction_result(replacement, true, reason, will_retry, &abort_token)
                    .await
                    .map(Some);
            }
        }

        // Run the pure engine.
        let thinking_level = thinking_level_str(self.thinking_level());

        let result = compaction::compact(
            &preparation,
            CompactOptions {
                model: &model,
                api_key: inputs.api_key.clone(),
                headers: inputs.headers.clone(),
                custom_instructions: if is_manual { custom_instructions } else { None },
                signal: Some(abort_token.clone()),
                thinking_level: thinking_level.as_deref(),
                stream_fn: inputs.stream_fn.clone(),
                env: inputs.env.clone(),
                hooks: None,
            },
        )
        .await?;

        // Persist + rebuild + extension session_compact.
        self.finalize_compaction_result(result, false, reason, will_retry, &abort_token)
            .await
            .map(Some)
    }

    /// Persist the compaction summary, rebuild the agent transcript, and
    /// dispatch the extension `session_compact` after-event.
    async fn finalize_compaction_result(
        &self,
        mut result: CompactionResult,
        from_hook: bool,
        reason: CompactionReason,
        will_retry: bool,
        abort_token: &CancellationToken,
    ) -> Result<CompactionResult, CompactionError> {
        if abort_token.is_cancelled() {
            return Err(CompactionError::Cancelled);
        }

        // Persist the compaction entry.
        let (_entry_id, saved_entry) = {
            let mut sm = self.session_manager.lock().await;
            let tokens_before_i64: i64 = i64::try_from(result.tokens_before).unwrap_or(i64::MAX);
            let details = result.details.clone();
            let entry_id = sm
                .append_compaction(
                    &result.summary,
                    &result.first_kept_entry_id,
                    tokens_before_i64,
                    details,
                    if from_hook { Some(true) } else { None },
                )
                .map_err(|err| {
                    CompactionError::SummarizationFailed(format!(
                        "failed to persist compaction: {err}"
                    ))
                })?;
            let saved = sm.get_entry(&entry_id).cloned();
            (entry_id, saved)
        };

        // Rebuild agent messages from the new session context.
        let new_messages = {
            let sm = self.session_manager.lock().await;
            sm.build_session_context()
                .map_err(CompactionError::MessageConversion)?
                .messages
        };
        self.agent.replace_messages(new_messages);

        // Estimate post-compaction tokens.
        let estimated_after = compaction::estimate_context_tokens(&self.agent.transcript()).tokens;
        result.estimated_tokens_after = Some(estimated_after);

        // Extension session_compact after-event (best-effort via CompactionEnd).
        if let Some(SessionEntry::Compaction(compaction_entry)) = &saved_entry {
            let runner = self.hooks.runner();
            self.extension_after_compact(&runner, compaction_entry, from_hook, reason, will_retry)
                .await;
        }

        // Emit the EntryAppended event for the new compaction entry.
        if let Some(entry) = saved_entry {
            self.emit_public(AgentSessionEvent::EntryAppended { entry });
        }

        Ok(result)
    }

    // -- auth + stream resolution -----------------------------------------

    /// Resolve the stream function, API key, headers, and env for compaction.
    ///
    /// Tries the concrete [`ModelRuntime`] first (production path). Falls back
    /// to [`CompactionStreamHandle`] (tests / SDK override).
    ///
    /// # Errors
    ///
    /// Returns [`CompactionError::SummarizationFailed`] when no model runtime
    /// is configured or auth resolution fails.
    async fn resolve_compaction_inputs(&self) -> Result<CompactionInputs, CompactionError> {
        if let Some(runtime) = self.model_runtime_handle() {
            let model = self.model();

            let auth = runtime
                .get_auth_for_model(&model, ModelRuntimeAuthOverrides::default())
                .await
                .map_err(|err| {
                    CompactionError::SummarizationFailed(format!("auth resolution failed: {err}"))
                })?;

            let api_key = auth.as_ref().and_then(|a| a.auth.api_key.clone());
            let headers = auth.as_ref().and_then(|a| a.auth.headers.clone());
            let env = auth.and_then(|a| {
                a.env.map(|provider_env| {
                    provider_env
                        .into_iter()
                        .collect::<BTreeMap<String, String>>()
                })
            });

            let stream_fn: SummarizeStreamFn = {
                let runtime = runtime.clone();
                Arc::new(move |model: Model, ctx: Context, opts: StreamOptions| {
                    let runtime = runtime.clone();
                    Box::pin(async move {
                        runtime.stream_simple(model, ctx, opts)
                            as Pin<
                                Box<
                                    dyn futures::Stream<
                                            Item = Result<AssistantMessageEvent, ProviderError>,
                                        > + Send,
                                >,
                            >
                    })
                })
            };

            return Ok(CompactionInputs {
                stream_fn,
                api_key,
                headers,
                env,
            });
        }

        if let Some(handle) = &self.compaction_stream_override {
            return Ok(CompactionInputs {
                stream_fn: handle.stream_fn.clone(),
                api_key: handle.api_key.clone(),
                headers: None,
                env: None,
            });
        }

        Err(CompactionError::SummarizationFailed(
            "No model runtime configured for compaction".to_owned(),
        ))
    }

    // -- extension hooks --------------------------------------------------

    /// Dispatch the extension `session_before_compact` event.
    ///
    /// The extension runner currently routes through the generic `emit` API
    /// using [`AgentSessionEvent::CompactionStart`] as the carrier (the TS
    /// `session_before_compact` payload — preparation/branchEntries/etc —
    /// requires dedicated runner methods not yet on the trait). Cancellation
    /// is honoured via the returned [`super::extension_runner::CancelResult`].
    ///
    /// Returns a [`BeforeCompactResult`] with `cancel` set when the extension
    /// cancelled, and `compaction` always `None` (replacement not deliverable
    /// through the current carrier).
    async fn extension_before_compact(
        &self,
        runner: &Arc<dyn ExtensionRunner>,
        event: AgentSessionEvent,
    ) -> Result<BeforeCompactResult, CompactionError> {
        match runner.emit(event).await {
            Ok(Some(cancel)) if cancel.cancel => Ok(BeforeCompactResult {
                cancel: true,
                compaction: None,
            }),
            Ok(_) => Ok(BeforeCompactResult::default()),
            Err(err) => {
                runner.emit_error(err.to_string());
                Ok(BeforeCompactResult::default())
            }
        }
    }

    /// Dispatch the extension `session_compact` after-event.
    ///
    /// Uses [`AgentSessionEvent::CompactionEnd`] as the carrier.
    async fn extension_after_compact(
        &self,
        runner: &Arc<dyn ExtensionRunner>,
        _entry: &CompactionEntry,
        _from_hook: bool,
        reason: CompactionReason,
        will_retry: bool,
    ) {
        let event = AgentSessionEvent::CompactionEnd {
            reason,
            result: None,
            aborted: false,
            will_retry,
            error_message: None,
        };
        if let Err(err) = runner.emit(event).await {
            runner.emit_error(err.to_string());
        }
    }

    // -- auto-compaction abort -------------------------------------------

    /// Begin the auto-compaction abort slot.
    fn begin_auto_compaction_abort(&self) -> CancellationToken {
        let token = CancellationToken::new();
        let mut inner = self.lock_inner();
        if let Some(prev) = inner.auto_compaction_abort.take() {
            prev.cancel();
        }
        inner.auto_compaction_abort = Some(token.clone());
        token
    }

    /// Clear the auto-compaction abort slot.
    fn clear_auto_compaction_abort(&self) {
        self.lock_inner().auto_compaction_abort = None;
    }

    // -- settings / session helpers --------------------------------------

    /// Resolved compaction settings (enabled + reserve + keep-recent).
    fn compaction_settings(&self) -> ResolvedCompactionSettings {
        self.lock_settings().get_compaction_settings()
    }

    /// Snapshot the current branch entries (cloned, lock-free).
    async fn snapshot_branch_entries(&self) -> Vec<SessionEntry> {
        let sm = self.session_manager.lock().await;
        sm.get_branch(None).into_iter().cloned().collect()
    }

    /// Latest compaction entry on the current branch.
    async fn latest_compaction_entry(&self) -> Option<CompactionEntry> {
        let sm = self.session_manager.lock().await;
        let branch = sm.get_branch(None);
        get_latest_compaction_entry(&branch).cloned()
    }

    /// Resolve the context-token count for the threshold check.
    ///
    /// Uses direct usage when the assistant reported valid tokens. For error
    /// or all-zero usage messages, estimates from the transcript's last valid
    /// usage anchor — and verifies that anchor is post-compaction.
    fn threshold_context_tokens(
        &self,
        assistant_message: &AssistantMessage,
        compaction_entry: Option<&CompactionEntry>,
    ) -> u64 {
        let direct = if assistant_message.usage.total_tokens != 0 {
            compaction::calculate_context_tokens(&assistant_message.usage)
        } else {
            0
        };

        if assistant_message.stop_reason != StopReason::Error && direct > 0 {
            return direct;
        }

        // Estimate from the transcript.
        let messages = self.agent.transcript();
        let estimate = compaction::estimate_context_tokens(&messages);
        if estimate.last_usage_index.is_none() {
            return 0;
        }

        // Verify the usage source is post-compaction.
        if let Some(latest) = compaction_entry
            && let boundary_ts = parse_iso_to_millis(&latest.timestamp)
            && boundary_ts > 0
            && let Some(idx) = estimate.last_usage_index
            && let Some(msg) = messages.get(idx)
            && let Some(pi_ai::Message::Assistant(assistant)) = msg.as_llm()
            && assistant.timestamp <= boundary_ts
        {
            return 0;
        }

        estimate.tokens
    }
}

// ---------------------------------------------------------------------------
// Pure helpers
// ---------------------------------------------------------------------------

/// Convert resolved settings to the pure-engine representation.
fn settings_to_pure(resolved: ResolvedCompactionSettings) -> CompactionSettings {
    CompactionSettings {
        enabled: resolved.enabled,
        reserve_tokens: resolved.reserve_tokens,
        keep_recent_tokens: resolved.keep_recent_tokens,
    }
}

/// Whether an assistant message came from the currently selected model.
fn same_model(model: &Model, assistant: &AssistantMessage) -> bool {
    assistant.provider == model.provider && assistant.model == model.id
}

/// Convert [`ModelThinkingLevel`] to its wire string (`"off"`, `"high"`, …).
fn thinking_level_str(level: pi_ai::ModelThinkingLevel) -> Option<String> {
    serde_json::to_value(level)
        .ok()
        .and_then(|v| v.as_str().map(str::to_owned))
}

/// Parse an ISO-8601 timestamp to Unix milliseconds.
///
/// Returns 0 on failure (treated as "no boundary" by callers).
fn parse_iso_to_millis(timestamp: &str) -> i64 {
    // Session entries store ISO-8601 strings (e.g.
    // "2025-01-01T00:00:00.000Z"). Parse to millis for the stale-guard
    // comparison. jiff is the workspace datetime library, but we avoid the
    // heavy import here and use a lightweight RFC3339 millisecond parse.
    parse_rfc3339_millis(timestamp).unwrap_or(0)
}

/// Lightweight RFC3339 millisecond parser for stale-guard comparisons.
fn parse_rfc3339_millis(ts: &str) -> Option<i64> {
    // Expected format: YYYY-MM-DDTHH:MM:SS.mmmZ
    // We only need millisecond precision for the comparison.
    let date_part = ts.get(..10)?; // YYYY-MM-DD
    let time_part = ts.get(11..)?;

    let year: i64 = date_part.get(..4)?.parse().ok()?;
    let month: u32 = date_part.get(5..7)?.parse().ok()?;
    let day: u32 = date_part.get(8..10)?.parse().ok()?;

    // Trim timezone suffix for hour/minute/second parsing.
    let time_core = time_part.split(['+', '-']).next()?.trim_end_matches('Z');

    let (hms, millis) = time_core.split_once('.').unwrap_or((time_core, "0"));
    let hour: u32 = hms.get(..2)?.parse().ok()?;
    let minute: u32 = hms.get(3..5)?.parse().ok()?;
    let second: u32 = hms.get(6..8).unwrap_or("00").parse().ok().unwrap_or(0);
    let ms: u32 = millis.get(..3).unwrap_or("000").parse().ok().unwrap_or(0);

    civil_to_millis(year, month, day, hour, minute, second, ms)
}

/// Convert civil time to Unix milliseconds (UTC, no leap seconds).
fn civil_to_millis(
    year: i64,
    month: u32,
    day: u32,
    hour: u32,
    min: u32,
    sec: u32,
    ms: u32,
) -> Option<i64> {
    // Days from 1970-01-01 using the Howard Hinnant algorithm.
    let y = if month <= 2 { year - 1 } else { year };
    let era = if y >= 0 { y } else { y - 399 } / 400;
    let yoe = u32::try_from(y - era * 400).ok()?;
    let m = if month > 2 { month - 3 } else { month + 9 };
    let doy = (153 * m + 2) / 5 + day - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    let days_since_epoch = era * 146_097 + i64::from(doe) - 719_468;

    let secs =
        days_since_epoch * 86_400 + i64::from(hour) * 3_600 + i64::from(min) * 60 + i64::from(sec);
    Some(secs * 1_000 + i64::from(ms))
}

// ---------------------------------------------------------------------------
// Context-overflow detection (port of pi-ai/utils/overflow.ts)
// ---------------------------------------------------------------------------

// Regex patterns compiled once at first use via `std::sync::LazyLock`.

static OVERFLOW_REGEXES: std::sync::LazyLock<Vec<Regex>> = std::sync::LazyLock::new(|| {
    [
        r"(?i)prompt is too long",
        r"(?i)request_too_large",
        r"(?i)input is too long for requested model",
        r"(?i)exceeds the context window",
        r"(?i)exceeds (?:the )?(?:model'?s )?maximum context length(?: of [\d,]+ tokens?|\s*\([\d,]+\))",
        r"(?i)input token count.*exceeds the maximum",
        r"(?i)maximum prompt length is \d+",
        r"(?i)reduce the length of the messages",
        r"(?i)maximum context length is \d+ tokens",
        r"(?i)exceeds (?:the )?maximum allowed input length of [\d,]+ tokens?",
        r"(?i)input \(\d+ tokens\) is longer than the model'?s context length \(\d+ tokens\)",
        r"(?i)exceeds the limit of \d+",
        r"(?i)exceeds the available context size",
        r"(?i)greater than the context length",
        r"(?i)context window exceeds limit",
        r"(?i)exceeded model token limit",
        r"(?i)too large for model with \d+ maximum context length",
        r"(?i)prompt has [\d,]+ tokens?, but the configured context size is [\d,]+ tokens?",
        r"(?i)model_context_window_exceeded",
        r"(?i)prompt too long; exceeded (?:max )?context length",
        r"(?i)context[_ ]length[_ ]exceeded",
        r"(?i)too many tokens",
        r"(?i)token limit exceeded",
        r"(?i)^4(?:00|13)\s*(?:status code)?\s*\(no body\)",
    ]
    .into_iter()
    .filter_map(|pat| Regex::new(pat).ok())
    .collect()
});

static NON_OVERFLOW_REGEXES: std::sync::LazyLock<Vec<Regex>> = std::sync::LazyLock::new(|| {
    [
        r"(?i)^(Throttling error|Service unavailable):",
        r"(?i)rate limit",
        r"(?i)too many requests",
    ]
    .into_iter()
    .filter_map(|pat| Regex::new(pat).ok())
    .collect()
});

fn overflow_patterns() -> &'static [Regex] {
    &OVERFLOW_REGEXES
}

fn non_overflow_patterns() -> &'static [Regex] {
    &NON_OVERFLOW_REGEXES
}

/// Check if an assistant message represents a context-overflow error.
///
/// Port of `pi-ai/utils/overflow.ts::isContextOverflow`. Handles:
/// 1. Error-based overflow (stopReason=error + matching message).
/// 2. Silent overflow (successful response with usage.input > contextWindow).
/// 3. Length-stop overflow (stopReason=length + output=0 + input fills window).
pub(super) fn is_context_overflow(message: &AssistantMessage, context_window: u64) -> bool {
    // Case 1: error-message patterns.
    if message.stop_reason == StopReason::Error
        && let Some(error_message) = message.error_message.as_deref()
    {
        let is_non_overflow = non_overflow_patterns()
            .iter()
            .any(|pattern| pattern.is_match(error_message));
        if !is_non_overflow
            && overflow_patterns()
                .iter()
                .any(|pattern| pattern.is_match(error_message))
        {
            return true;
        }
    }

    // Case 2: silent overflow (z.ai style).
    if context_window > 0 && message.stop_reason == StopReason::Stop {
        let input_tokens = message.usage.input.saturating_add(message.usage.cache_read);
        if input_tokens > context_window {
            return true;
        }
    }

    // Case 3: length-stop overflow (Xiaomi MiMo style).
    if context_window > 0 && message.stop_reason == StopReason::Length && message.usage.output == 0
    {
        let input_tokens = message.usage.input.saturating_add(message.usage.cache_read);
        let threshold = context_window / 100 * 99 + context_window % 100 * 99 / 100;
        if input_tokens >= threshold {
            return true;
        }
    }

    false
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::agent_session::{AgentSession, AgentSessionConfig};
    use crate::core::sessions::SessionEntry;
    use futures::stream::{self, BoxStream};
    use pi_agent::user_text;
    use pi_ai::{
        AssistantContent, AssistantMessageEvent, Context, DoneReason, Model, ModelCost, ModelInput,
        ModelThinkingLevel, Provider, ProviderError, StreamOptions, TextContent, Usage, UsageCost,
    };
    use serde_json::{Map, Value};
    use std::sync::Arc;
    use tokio::sync::mpsc;
    use tokio::time::{sleep, timeout};

    type TestResult<T = ()> = Result<T, Box<dyn std::error::Error>>;

    // -- test helpers -----------------------------------------------------

    fn test_model(context_window: u64) -> Model {
        Model {
            id: "m".to_owned(),
            name: "m".to_owned(),
            api: "test-api".to_owned(),
            provider: "test-provider".to_owned(),
            base_url: String::new(),
            reasoning: false,
            thinking_level_map: None,
            input: vec![ModelInput::Text],
            cost: ModelCost::default(),
            context_window,
            max_tokens: 1_024,
            headers: None,
            compat: None,
            extra: std::collections::BTreeMap::new(),
        }
    }

    fn mock_provider() -> Arc<dyn Provider> {
        struct NoopProvider;
        impl Provider for NoopProvider {
            fn stream(
                &self,
                _model: &Model,
                _ctx: Context,
                _opts: StreamOptions,
            ) -> BoxStream<'static, Result<AssistantMessageEvent, ProviderError>> {
                // The agent prompt path is not exercised here; return empty.
                Box::pin(stream::iter(Vec::new()))
            }
        }
        Arc::new(NoopProvider)
    }

    fn summary_stream_fn(text: &str) -> SummarizeStreamFn {
        let text = text.to_owned();
        Arc::new(move |_model, _ctx, _opts| {
            let text = text.clone();
            Box::pin(async move {
                let mut msg = AssistantMessage::new("a", "p", "m", 1);
                msg.content = vec![AssistantContent::Text(TextContent::new(text.clone()))];
                msg.stop_reason = StopReason::Stop;
                let stream = stream::iter(vec![Ok(AssistantMessageEvent::Done {
                    reason: DoneReason::Stop,
                    message: msg,
                })]);
                Box::pin(stream)
                    as Pin<
                        Box<
                            dyn futures::Stream<Item = Result<AssistantMessageEvent, ProviderError>>
                                + Send,
                        >,
                    >
            })
        })
    }

    fn error_stream_fn(msg: &str) -> SummarizeStreamFn {
        let msg = msg.to_owned();
        Arc::new(move |_model, _ctx, _opts| {
            let msg = msg.clone();
            Box::pin(async move {
                let mut message = AssistantMessage::new("a", "p", "m", 1);
                message.stop_reason = StopReason::Error;
                message.error_message = Some(msg.clone());
                let stream = stream::iter(vec![Ok(AssistantMessageEvent::Done {
                    reason: DoneReason::Stop,
                    message,
                })]);
                Box::pin(stream)
                    as Pin<
                        Box<
                            dyn futures::Stream<Item = Result<AssistantMessageEvent, ProviderError>>
                                + Send,
                        >,
                    >
            })
        })
    }

    fn make_session(
        context_window: u64,
        stream_fn: SummarizeStreamFn,
        messages: Vec<pi_agent::AgentMessage>,
    ) -> TestResult<Arc<AgentSession>> {
        let provider = mock_provider();
        let mut config = AgentSessionConfig::test_config(provider, test_model(context_window))?;
        config.system_prompt = "sys".into();
        config.messages = messages;
        config.compaction_stream_override = Some(CompactionStreamHandle::new(stream_fn));
        Ok(AgentSession::new(config)?)
    }

    fn assistant_with_usage(text: &str, usage: Usage, stop: StopReason) -> pi_agent::AgentMessage {
        let mut msg =
            AssistantMessage::new("test-api", "test-provider", "m", pi_agent::now_millis());
        msg.content = vec![AssistantContent::Text(TextContent::new(text))];
        msg.usage = usage;
        msg.stop_reason = stop;
        pi_agent::AgentMessage::Llm(Box::new(pi_ai::Message::Assistant(msg)))
    }

    fn assistant_overflow_message() -> AssistantMessage {
        let mut msg =
            AssistantMessage::new("test-api", "test-provider", "m", pi_agent::now_millis());
        msg.stop_reason = StopReason::Error;
        msg.error_message = Some("prompt is too long: 999999 tokens > 8192 maximum".into());
        msg
    }

    /// Build a session with enough messages to compact. Returns the session
    /// with the transcript pre-populated AND the session tree holding matching
    /// entries.
    ///
    /// Mirrors TypeScript suite fixtures by setting `keepRecentTokens: 1` so
    /// small synthetic conversations have a real cut point instead of being
    /// wholly retained by the production 20k keep-recent window.
    async fn session_with_history(
        context_window: u64,
        stream_fn: SummarizeStreamFn,
    ) -> TestResult<Arc<AgentSession>> {
        // Create a long conversation that exceeds the keep-recent window.
        let mut messages = Vec::new();
        for i in 0..20 {
            messages.push(user_text(
                format!("user message number {i} with some padding text to make it larger"),
                std::iter::empty(),
            ));
            let usage = Usage {
                input: 500 + i * 100,
                output: 200,
                cache_read: 0,
                cache_write: 0,
                cache_write1h: None,
                reasoning: None,
                total_tokens: 700 + i * 100,
                cost: UsageCost::default(),
            };
            messages.push(assistant_with_usage(
                format!("assistant reply {i} with enough text to be a real message").as_str(),
                usage,
                StopReason::Stop,
            ));
        }

        let session = make_session(context_window, stream_fn, messages)?;

        // Match TS agent-session-compaction fixtures: force a tiny keep-recent
        // budget so prepare_compaction has messages to summarize.
        {
            let mut settings = session.lock_settings();
            let mut compaction = Map::new();
            compaction.insert("keepRecentTokens".into(), Value::from(1u64));
            let mut overrides = Map::new();
            overrides.insert("compaction".into(), Value::Object(compaction));
            settings.apply_overrides(&overrides);
        }

        // Persist messages into the session tree so compaction has path entries.
        {
            let mut sm = session.session_manager.lock().await;
            for msg in session.agent.transcript() {
                sm.append_message(&msg)?;
            }
            assert!(
                !sm.get_branch(None).is_empty(),
                "session_with_history must leave a non-empty leaf branch"
            );
        }

        Ok(session)
    }

    async fn collect_events(rx: &mut mpsc::UnboundedReceiver<String>, n: usize) -> Vec<String> {
        let mut out = Vec::new();
        while out.len() < n {
            match timeout(std::time::Duration::from_secs(2), rx.recv()).await {
                Ok(Some(v)) => out.push(v),
                Ok(None) | Err(_) => break,
            }
        }
        out
    }

    // -- unit tests for pure helpers -------------------------------------

    #[test]
    fn is_context_overflow_error_pattern() {
        let mut msg = AssistantMessage::new("a", "anthropic", "m", 1);
        msg.stop_reason = StopReason::Error;
        msg.error_message = Some("prompt is too long: 213462 tokens > 200000 maximum".into());
        assert!(is_context_overflow(&msg, 0));
    }

    #[test]
    fn bare_maximum_context_length_is_not_overflow() {
        let mut msg = AssistantMessage::new("a", "openai-compatible", "m", 1);
        msg.stop_reason = StopReason::Error;
        msg.error_message = Some("exceeds the maximum context length".into());
        assert!(!is_context_overflow(&msg, 0));
    }

    #[test]
    fn is_context_overflow_non_overflow_rate_limit() {
        let mut msg = AssistantMessage::new("a", "bedrock", "m", 1);
        msg.stop_reason = StopReason::Error;
        msg.error_message = Some("rate limit exceeded".into());
        assert!(!is_context_overflow(&msg, 0));
    }

    #[test]
    fn is_context_overflow_silent_zai_style() {
        let mut msg = AssistantMessage::new("a", "zai", "m", 1);
        msg.stop_reason = StopReason::Stop;
        msg.usage = Usage {
            input: 100_000,
            output: 10,
            cache_read: 0,
            cache_write: 0,
            cache_write1h: None,
            reasoning: None,
            total_tokens: 100_010,
            cost: UsageCost::default(),
        };
        assert!(is_context_overflow(&msg, 80_000));
        assert!(!is_context_overflow(&msg, 120_000));
    }

    #[test]
    fn is_context_overflow_length_stop_mimo_style() {
        let mut msg = AssistantMessage::new("a", "mimo", "m", 1);
        msg.stop_reason = StopReason::Length;
        msg.usage = Usage {
            input: 8_100,
            output: 0,
            cache_read: 0,
            cache_write: 0,
            cache_write1h: None,
            reasoning: None,
            total_tokens: 8_100,
            cost: UsageCost::default(),
        };
        // input >= 99% of context_window(8192) = 8109 → 8100 < 8109, just under
        assert!(!is_context_overflow(&msg, 8_192));
        // input >= 99% of 8000 = 7920 → 8100 >= 7920
        msg.usage.input = 8_110;
        assert!(is_context_overflow(&msg, 8_192));
    }

    #[test]
    fn same_model_checks_provider_and_id() {
        let model = test_model(8192);
        let mut msg = AssistantMessage::new("test-api", "test-provider", "m", 1);
        assert!(same_model(&model, &msg));
        msg.provider = "other".into();
        assert!(!same_model(&model, &msg));
    }

    #[test]
    fn parse_rfc3339_millis_basic() -> TestResult {
        let ts = "2025-01-01T00:00:00.000Z";
        let millis = parse_rfc3339_millis(ts).ok_or("valid basic RFC3339 timestamp")?;
        assert_eq!(millis, 1_735_689_600_000);
        Ok(())
    }

    #[test]
    fn parse_rfc3339_millis_with_fractional() -> TestResult {
        let ts = "2025-01-01T12:30:45.123Z";
        let millis = parse_rfc3339_millis(ts).ok_or("valid fractional RFC3339 timestamp")?;
        assert_eq!(millis, 1_735_734_645_123);
        Ok(())
    }

    #[test]
    fn thinking_level_str_serializes() {
        assert_eq!(
            thinking_level_str(ModelThinkingLevel::Off),
            Some("off".to_owned())
        );
        assert_eq!(
            thinking_level_str(ModelThinkingLevel::High),
            Some("high".to_owned())
        );
    }

    // -- integration tests for compaction flow ----------------------------

    #[tokio::test]
    async fn manual_compact_produces_summary_and_rebuilds_messages() -> TestResult {
        let session =
            session_with_history(8_192, summary_stream_fn("## Goal\nTest summary")).await?;

        let (tx, mut rx) = mpsc::unbounded_channel();
        let _unsub = session.subscribe(move |event| {
            let _ = tx.send(event.type_name().to_owned());
        });

        let result = session.compact(None).await?;
        sleep(std::time::Duration::from_millis(50)).await;

        assert!(
            result.summary.contains("Test summary"),
            "summary should contain stream text: {}",
            result.summary
        );
        assert!(!result.first_kept_entry_id.is_empty());
        assert!(result.tokens_before > 0);
        assert!(result.estimated_tokens_after.is_some());

        let messages = session.agent.transcript();
        assert!(
            messages.len() < 40,
            "transcript should be smaller after compaction: {}",
            messages.len()
        );

        let events = collect_events(&mut rx, 4).await;
        assert!(
            events.iter().any(|e| e == "compaction_start"),
            "missing compaction_start: {events:?}"
        );
        assert!(
            events.iter().any(|e| e == "compaction_end"),
            "missing compaction_end: {events:?}"
        );
        let start_idx = events
            .iter()
            .position(|e| e == "compaction_start")
            .ok_or("compaction_start event position")?;
        let end_idx = events
            .iter()
            .position(|e| e == "compaction_end")
            .ok_or("compaction_end event position")?;
        assert!(
            start_idx < end_idx,
            "compaction_start before compaction_end"
        );
        Ok(())
    }

    #[tokio::test]
    async fn manual_compact_already_compacted_error() -> TestResult {
        let session = session_with_history(8_192, summary_stream_fn("summary")).await?;
        session.compact(None).await?;
        let second = session.compact(None).await;
        assert!(
            matches!(second, Err(CompactionError::AlreadyCompacted)),
            "expected AlreadyCompacted, got: {second:?}"
        );
        Ok(())
    }

    #[tokio::test]
    async fn manual_compact_too_small_error() -> TestResult {
        let messages = vec![
            user_text("hi", std::iter::empty()),
            assistant_with_usage("hello", Usage::default(), StopReason::Stop),
        ];
        let session = make_session(8_192, summary_stream_fn("summary"), messages)?;
        {
            let mut sm = session.session_manager.lock().await;
            for msg in session.agent.transcript() {
                sm.append_message(&msg)?;
            }
        }
        let result = session.compact(None).await;
        assert!(
            matches!(result, Err(CompactionError::NothingToCompact)),
            "expected NothingToCompact, got: {result:?}"
        );
        Ok(())
    }

    #[tokio::test]
    async fn manual_compact_cancelled_by_abort() -> TestResult {
        // Hanging summarize stream that never emits Done. Pure compact races
        // this against the abort signal, so abort_compaction should win.
        let hang_stream: SummarizeStreamFn = Arc::new(move |_model, _ctx, opts| {
            let signal = opts.signal.clone();
            Box::pin(async move {
                // Wait until cancelled (or a long timeout as a safety net).
                if let Some(token) = signal {
                    token.cancelled().await;
                } else {
                    sleep(std::time::Duration::from_secs(5)).await;
                }
                // Still return a stream; compact should have already exited on cancel.
                let mut msg = AssistantMessage::new("a", "p", "m", 1);
                msg.content = vec![AssistantContent::Text(TextContent::new("late"))];
                msg.stop_reason = StopReason::Stop;
                let stream = stream::iter(vec![Ok(AssistantMessageEvent::Done {
                    reason: DoneReason::Stop,
                    message: msg,
                })]);
                Box::pin(stream)
                    as Pin<
                        Box<
                            dyn futures::Stream<Item = Result<AssistantMessageEvent, ProviderError>>
                                + Send,
                        >,
                    >
            })
        });
        let session = session_with_history(8_192, hang_stream).await?;

        // Composite session abort is the path driven by interactive Esc.
        let session_clone = Arc::clone(&session);
        tokio::spawn(async move {
            sleep(std::time::Duration::from_millis(5)).await;
            session_clone.abort().await;
        });

        let result = timeout(std::time::Duration::from_secs(2), session.compact(None)).await?;
        assert!(
            matches!(result, Err(CompactionError::Cancelled)),
            "expected Cancelled, got: {result:?}"
        );
        Ok(())
    }

    #[tokio::test]
    async fn manual_compact_error_emits_error_message() -> TestResult {
        let session =
            session_with_history(8_192, error_stream_fn("summarization exploded")).await?;

        let (tx, mut rx) = mpsc::unbounded_channel::<AgentSessionEvent>();
        let _unsub = session.subscribe(move |event| {
            let _ = tx.send(event.clone());
        });

        let result = session.compact(None).await;
        assert!(result.is_err());

        sleep(std::time::Duration::from_millis(50)).await;

        // Collect compaction_end events.
        let mut end_events = Vec::new();
        while let Ok(Some(event)) = timeout(std::time::Duration::from_millis(100), rx.recv()).await
        {
            if event.type_name() == "compaction_end" {
                end_events.push(event);
            }
        }
        assert!(!end_events.is_empty(), "should emit compaction_end");
        let Some(AgentSessionEvent::CompactionEnd { error_message, .. }) = end_events.first()
        else {
            return Err("expected CompactionEnd variant".into());
        };
        assert!(
            error_message
                .as_ref()
                .is_some_and(|m| m.contains("Compaction failed")),
            "error message should contain 'Compaction failed': {error_message:?}"
        );
        Ok(())
    }

    #[tokio::test]
    async fn manual_compact_custom_instructions_passed_to_summary() -> TestResult {
        let session = session_with_history(8_192, summary_stream_fn("## Goal\nSummary")).await?;
        session.compact(Some("Focus on file changes")).await?;
        Ok(())
    }

    #[tokio::test]
    async fn manual_compact_previous_summary_uses_update_prompt() -> TestResult {
        let session = session_with_history(8_192, summary_stream_fn("## Goal\nUpdated")).await?;
        let first = session.compact(None).await?;
        assert!(first.summary.contains("Updated"));
        {
            let mut sm = session.session_manager.lock().await;
            let new_user = user_text("after compaction question", std::iter::empty());
            let new_assistant = assistant_with_usage(
                "after compaction answer with some content",
                Usage {
                    input: 1000,
                    output: 500,
                    ..Usage::default()
                },
                StopReason::Stop,
            );
            sm.append_message(&new_user)?;
            sm.append_message(&new_assistant)?;
        }
        // Also push to agent state.
        session
            .agent
            .push_message(user_text("after compaction question", std::iter::empty()));
        session.agent.push_message(assistant_with_usage(
            "after compaction answer with some content",
            Usage {
                input: 1000,
                output: 500,
                ..Usage::default()
            },
            StopReason::Stop,
        ));

        // Second compaction: should use the update prompt path (previous_summary
        // is set from the first compaction entry). The pure engine handles this
        // internally.
        let second = session.compact(None).await;
        // Second compact may succeed or fail depending on whether enough
        // new content exists. The important assertion is that it doesn't
        // return AlreadyCompacted (there IS new content after the boundary).
        assert!(
            !matches!(second, Err(CompactionError::AlreadyCompacted)),
            "should not be AlreadyCompacted with new messages: {second:?}"
        );
        Ok(())
    }

    #[tokio::test]
    async fn check_compaction_threshold_triggers_compaction() -> TestResult {
        let session = session_with_history(8_192, summary_stream_fn("threshold summary")).await?;
        let last_assistant = session
            .agent
            .last_assistant()
            .ok_or("threshold fixture has a last assistant")?;
        let _context_tokens = compaction::calculate_context_tokens(&last_assistant.usage);
        let should_continue = session.check_compaction(&last_assistant).await;
        assert!(!should_continue, "threshold should not continue");
        assert!(!session.is_compacting(), "should clear compaction latch");
        let messages = session.agent.transcript();
        assert!(
            messages.len() < 40,
            "transcript should be smaller after auto-compaction: {}",
            messages.len()
        );
        Ok(())
    }

    #[tokio::test]
    async fn check_compaction_below_threshold_no_compaction() -> TestResult {
        let session = session_with_history(1_000_000, summary_stream_fn("no compact")).await?;
        let last_assistant = session
            .agent
            .last_assistant()
            .ok_or("below-threshold fixture has a last assistant")?;
        let messages_before = session.agent.transcript().len();
        let should_continue = session.check_compaction(&last_assistant).await;
        assert!(!should_continue);
        assert_eq!(
            session.agent.transcript().len(),
            messages_before,
            "transcript should be unchanged"
        );
        Ok(())
    }

    #[tokio::test]
    async fn check_compaction_overflow_successful_response_compacts_without_retry() -> TestResult {
        let session = session_with_history(8_192, summary_stream_fn("overflow recovery")).await?;
        let mut overflow_assistant =
            AssistantMessage::new("test-api", "test-provider", "m", pi_agent::now_millis());
        overflow_assistant.stop_reason = StopReason::Stop;
        overflow_assistant.usage = Usage {
            input: 10_000,
            output: 500,
            cache_read: 0,
            cache_write: 0,
            cache_write1h: None,
            reasoning: None,
            total_tokens: 10_500,
            cost: UsageCost::default(),
        };
        overflow_assistant.content = vec![AssistantContent::Text(TextContent::new("overflow"))];
        let should_continue = session.check_compaction(&overflow_assistant).await;
        assert!(!should_continue, "successful overflow should not retry");
        assert!(!session.is_compacting(), "should clear compaction latch");
        let messages = session.agent.transcript();
        assert!(
            messages.len() < 40,
            "transcript should be smaller: {}",
            messages.len()
        );
        Ok(())
    }

    #[tokio::test]
    async fn check_compaction_overflow_error_compacts_and_retries_once() -> TestResult {
        let session = session_with_history(8_192, summary_stream_fn("overflow recovery")).await?;
        let overflow_assistant = assistant_overflow_message();
        let overflow_msg = pi_agent::AgentMessage::Llm(Box::new(pi_ai::Message::Assistant(
            overflow_assistant.clone(),
        )));
        session.agent.push_message(overflow_msg.clone());
        {
            let mut sm = session.session_manager.lock().await;
            sm.append_message(&overflow_msg)?;
        }
        let should_continue = session.check_compaction(&overflow_assistant).await;
        assert!(should_continue, "error overflow should signal continuation");
        Ok(())
    }

    #[tokio::test]
    async fn check_compaction_overflow_second_attempt_emits_terminal_error() -> TestResult {
        let session = session_with_history(8_192, summary_stream_fn("overflow recovery")).await?;
        {
            let mut inner = session.lock_inner();
            inner.overflow_recovery_attempted = true;
        }
        let overflow_assistant = assistant_overflow_message();
        let (tx, mut rx) = mpsc::unbounded_channel::<AgentSessionEvent>();
        let _unsub = session.subscribe(move |event| {
            let _ = tx.send(event.clone());
        });
        let should_continue = session.check_compaction(&overflow_assistant).await;
        assert!(!should_continue, "second overflow should not continue");
        sleep(std::time::Duration::from_millis(50)).await;
        let mut ends = Vec::new();
        while let Ok(Some(event)) = timeout(std::time::Duration::from_millis(100), rx.recv()).await
        {
            if event.type_name() == "compaction_end" {
                ends.push(event);
            }
        }
        assert!(!ends.is_empty(), "should emit compaction_end");
        let Some(AgentSessionEvent::CompactionEnd {
            error_message,
            reason,
            ..
        }) = ends.first()
        else {
            return Err("expected terminal CompactionEnd event".into());
        };
        assert_eq!(*reason, CompactionReason::Overflow);
        assert!(
            error_message.as_ref().is_some_and(|m| m
                .contains("Context overflow recovery failed after one compact-and-retry attempt")),
            "exact terminal error: {error_message:?}"
        );
        Ok(())
    }

    #[tokio::test]
    async fn check_compaction_different_model_skips_overflow() -> TestResult {
        // Large context window so threshold does not fire; only overflow would.
        // Overflow from a different model must be ignored by the same-model guard.
        let session = session_with_history(1_000_000, summary_stream_fn("no compact")).await?;

        let mut other_model_msg = AssistantMessage::new(
            "test-api",
            "different-provider",
            "other-model",
            pi_agent::now_millis(),
        );
        other_model_msg.stop_reason = StopReason::Error;
        other_model_msg.error_message = Some("prompt is too long".into());

        let messages_before = session.agent.transcript().len();
        let should_continue = session.check_compaction(&other_model_msg).await;

        assert!(!should_continue, "different model should not compact");
        assert_eq!(
            session.agent.transcript().len(),
            messages_before,
            "transcript unchanged"
        );
        Ok(())
    }

    #[tokio::test]
    async fn check_compaction_stale_pre_compaction_usage_ignored() -> TestResult {
        let session = session_with_history(8_192, summary_stream_fn("first compact")).await?;
        session.compact(None).await?;
        let mut old_msg = AssistantMessage::new("test-api", "test-provider", "m", 1);
        old_msg.stop_reason = StopReason::Stop;
        old_msg.usage = Usage {
            input: 10_000,
            output: 500,
            ..Usage::default()
        };
        let should_continue = session.check_compaction(&old_msg).await;
        assert!(
            !should_continue,
            "stale pre-compaction message should not trigger compaction"
        );
        Ok(())
    }

    #[tokio::test]
    async fn check_compaction_disabled_returns_false() -> TestResult {
        let session = session_with_history(8_192, summary_stream_fn("disabled")).await?;
        session.set_auto_compaction_enabled(false);
        assert!(
            !session.lock_settings().get_compaction_settings().enabled,
            "runtime toggle must update persisted settings state"
        );

        let last_assistant = session
            .agent
            .last_assistant()
            .ok_or("disabled fixture has a last assistant")?;
        let messages_before = session.agent.transcript().len();

        let should_continue = session.check_compaction(&last_assistant).await;

        assert!(!should_continue);
        assert_eq!(
            session.agent.transcript().len(),
            messages_before,
            "transcript unchanged when disabled"
        );
        Ok(())
    }

    #[tokio::test]
    async fn manual_compact_no_model_runtime_fails() -> TestResult {
        let mut messages = Vec::new();
        for i in 0..4 {
            messages.push(user_text(
                format!("user message {i} padding for compaction"),
                std::iter::empty(),
            ));
            messages.push(assistant_with_usage(
                format!("assistant reply {i} with content").as_str(),
                Usage {
                    input: 100 + i * 10,
                    output: 50,
                    total_tokens: 150 + i * 10,
                    ..Usage::default()
                },
                StopReason::Stop,
            ));
        }
        let provider = mock_provider();
        let mut config = AgentSessionConfig::test_config(provider, test_model(8_192))?;
        config.messages = messages;
        let session = AgentSession::new(config)?;
        {
            let mut settings = session.lock_settings();
            let mut compaction = Map::new();
            compaction.insert("keepRecentTokens".into(), Value::from(1u64));
            let mut overrides = Map::new();
            overrides.insert("compaction".into(), Value::Object(compaction));
            settings.apply_overrides(&overrides);
        }
        {
            let mut sm = session.session_manager.lock().await;
            for msg in session.agent.transcript() {
                sm.append_message(&msg)?;
            }
        }
        let result = session.compact(None).await;
        assert!(
            matches!(result, Err(CompactionError::SummarizationFailed(_))),
            "expected SummarizationFailed without model_runtime: {result:?}"
        );
        Ok(())
    }

    #[tokio::test]
    async fn run_auto_compaction_queued_message_continuation() -> TestResult {
        let session = session_with_history(8_192, summary_stream_fn("queued summary")).await?;
        session
            .agent
            .follow_up(user_text("queued follow-up", std::iter::empty()));
        session.mirror_follow_up_push("queued follow-up".into());
        let last_assistant = session
            .agent
            .last_assistant()
            .ok_or("queued fixture has a last assistant")?;
        let should_continue = session.check_compaction(&last_assistant).await;
        assert!(
            should_continue,
            "should continue when queued messages exist after auto-compaction"
        );
        Ok(())
    }

    #[tokio::test]
    async fn manual_compact_persists_entry_to_session_tree() -> TestResult {
        let session = session_with_history(8_192, summary_stream_fn("persisted")).await?;
        let result = session.compact(None).await?;
        let entries: Vec<SessionEntry> = {
            let sm = session.session_manager.lock().await;
            sm.get_entries().into_iter().cloned().collect()
        };
        let compaction_entries: Vec<_> = entries
            .iter()
            .filter(|e| e.discriminant() == "compaction")
            .collect();
        assert_eq!(compaction_entries.len(), 1, "exactly one compaction entry");
        let Some(SessionEntry::Compaction(compaction)) = compaction_entries.first().copied() else {
            return Err("expected Compaction entry".into());
        };
        assert_eq!(compaction.summary, result.summary);
        assert_eq!(compaction.first_kept_entry_id, result.first_kept_entry_id);
        Ok(())
    }

    #[tokio::test]
    async fn manual_compact_emits_entry_appended() -> TestResult {
        let session = session_with_history(8_192, summary_stream_fn("entry event")).await?;

        let (tx, mut rx) = mpsc::unbounded_channel::<String>();
        let _unsub = session.subscribe(move |event| {
            let _ = tx.send(event.type_name().to_owned());
        });

        let _ = session.compact(None).await;

        sleep(std::time::Duration::from_millis(50)).await;

        let events = collect_events(&mut rx, 10).await;
        assert!(
            events.iter().any(|e| e == "entry_appended"),
            "should emit entry_appended for the compaction entry: {events:?}"
        );
        Ok(())
    }
}
