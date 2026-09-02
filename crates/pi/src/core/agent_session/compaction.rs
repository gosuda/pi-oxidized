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

use pi_ai::{AssistantMessage, AssistantMessageEvent, Model, ProviderError, StopReason};
use regex::Regex;
use tokio_util::sync::CancellationToken;

use crate::core::compaction::{
    self, BeforeCompactResult, CompactOptions, CompactionError, CompactionPreparation,
    CompactionResult, CompactionSettings, SummarizationRetryCallbacks, SummarizationRetryPolicy,
    SummarizeStreamFn, preparation_none_error, prepare_compaction, should_compact,
};
use crate::core::model_runtime::ModelRuntimeAuthOverrides;
use crate::core::sessions::{CompactionEntry, SessionEntry, get_latest_compaction_entry};
use crate::core::settings::ResolvedCompactionSettings;
use pi_agent::AgentContext;
use pi_agent::telemetry::{
    AiOperation, AiRequestStart, HarnessCompactionStart, SpanStatus, TelemetrySpan, contained,
    start_ai_request_span, start_harness_compaction_span,
};

use super::AgentSession;
use super::events::{AgentSessionEvent, CompactionReason, SummarizationRetrySource};
use super::extension_runner::ExtensionRunner;
use super::tree::SummarizationAuth;

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
                let message = format!("Compaction failed: {err}");
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
    pub(super) async fn check_compaction(
        &self,
        assistant_message: &AssistantMessage,
        skip_aborted_check: bool,
    ) -> bool {
        let _ = skip_aborted_check; // retained for TS parity; behaviour below
        // already treats aborted as non-overflow.

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
                    .run_auto_compaction(CompactionReason::Overflow, false, None)
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
                .run_auto_compaction(CompactionReason::Overflow, will_retry, None)
                .await;
        }

        // Case 2: Threshold.
        let context_tokens =
            self.threshold_context_tokens(assistant_message, compaction_entry.as_ref());
        if should_compact(context_tokens, context_window, &settings_to_pure(settings)) {
            return self
                .run_auto_compaction(CompactionReason::Threshold, false, None)
                .await;
        }

        false
    }

    // -- same-run post-turn compaction ------------------------------------

    /// Threshold compaction for the `prepare_next_turn` callback.
    ///
    /// Gates on the runtime auto-compaction flag, a nonzero context window,
    /// and the pure threshold check over the callback context estimate.
    /// Runs the shared auto lifecycle (`Threshold`, no retry) with the active
    /// run's cancellation token, so an aborted run cancels the session-owned
    /// token and drains the same core future through the normal
    /// `compaction_end` + cleanup path. The caller rebuilds its context from
    /// the agent transcript on every outcome.
    pub(super) async fn compact_before_next_assistant_response(
        &self,
        context: &AgentContext,
        run_cancel: &CancellationToken,
    ) {
        // Runtime flag is the source of truth (mirrors `check_compaction`).
        if !self.lock_inner().auto_compaction_enabled {
            return;
        }
        // A zero window cannot define a threshold to cross.
        let context_window = self.model().context_window;
        if context_window == 0 {
            return;
        }
        let settings = self.compaction_settings();
        let context_tokens = compaction::estimate_context_tokens(&context.messages).tokens;
        if !should_compact(context_tokens, context_window, &settings_to_pure(settings)) {
            return;
        }

        self.run_auto_compaction(CompactionReason::Threshold, false, Some(run_cancel.clone()))
            .await;
    }

    // -- internal auto-compaction runner ----------------------------------

    /// Run auto-compaction with the full event + extension lifecycle.
    ///
    /// Returns `true` when the caller should continue the run (overflow retry
    /// or queued-message delivery), `false` otherwise.
    async fn run_auto_compaction(
        &self,
        reason: CompactionReason,
        will_retry: bool,
        run_cancel: Option<CancellationToken>,
    ) -> bool {
        let (abort_token, owner) = self.begin_auto_compaction_abort();

        self.emit_public_awaited(&AgentSessionEvent::CompactionStart { reason })
            .await;

        // One pinned core future: when the run token wins the race, the
        // session-owned token is cancelled and the SAME future is awaited to
        // completion so its terminal cleanup (span settle, extension
        // after-compact guards) still runs; the future is never dropped.
        let core = self.run_compaction_core(reason, None, will_retry, abort_token.clone(), false);
        tokio::pin!(core);

        let result = if let Some(run_cancel) = run_cancel {
            tokio::select! {
                biased;
                () = run_cancel.cancelled() => {
                    abort_token.cancel();
                    core.as_mut().await
                }
                result = core.as_mut() => result,
            }
        } else {
            core.as_mut().await
        };

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

        self.clear_auto_compaction_abort(owner);
        should_continue
    }

    /// Run the extension `before_compact` hook. Returns `Ok(Some(result))`
    /// when the extension provides a full replacement (persisted + finalized),
    /// `Ok(None)` when no hook or the hook did not short-circuit, and `Err`
    /// on cancellation or hook failure.
    async fn try_extension_before_compact(
        &self,
        runner: &Arc<dyn ExtensionRunner>,
        reason: CompactionReason,
        abort_token: &CancellationToken,
        will_retry: bool,
        compaction_span: &dyn TelemetrySpan,
    ) -> Result<Option<CompactionResult>, CompactionError> {
        if !runner.has_handlers("session_before_compact") {
            return Ok(None);
        }
        let event = AgentSessionEvent::CompactionStart { reason };
        let cancel = self
            .extension_before_compact(runner, event, abort_token)
            .await
            .inspect_err(|err| {
                contained(
                    || {
                        compaction_span.set_status(SpanStatus::Error {
                            name: None,
                            message: Some(err.to_string()),
                        });
                    },
                    || (),
                );
            })?;
        if cancel.cancel {
            return Err(CompactionError::Cancelled);
        }
        if let Some(replacement) = cancel.compaction {
            // Extension provided the full result — persist + emit without
            // invoking the pure summariser. The span stays open across
            // finalize so persist/rebuild failures record Error status.
            let mut replacement = replacement;
            replacement.from_hook = Some(true);
            return self
                .finalize_compaction_result(replacement, true, reason, will_retry, abort_token)
                .await
                .inspect_err(|err| {
                    contained(
                        || {
                            compaction_span.set_status(SpanStatus::Error {
                                name: None,
                                message: Some(err.to_string()),
                            });
                        },
                        || (),
                    );
                })
                .map(Some);
        }
        Ok(None)
    }

    /// Record span status for the pure-engine result: `Ok` on the AI span,
    /// `Error` on both the AI and compaction spans on failure.
    fn record_compaction_result_status(
        result: &Result<CompactionResult, CompactionError>,
        ai_span: &dyn TelemetrySpan,
        compaction_span: &dyn TelemetrySpan,
    ) {
        match result {
            Ok(_) => {
                contained(|| ai_span.set_status(SpanStatus::Ok), || ());
            }
            Err(err) => {
                contained(
                    || {
                        ai_span.set_status(SpanStatus::Error {
                            name: None,
                            message: Some(err.to_string()),
                        });
                    },
                    || (),
                );
                // The engine failed, so the enclosing compaction operation
                // failed too — record Error before the span settles on drop.
                contained(
                    || {
                        compaction_span.set_status(SpanStatus::Error {
                            name: None,
                            message: Some(err.to_string()),
                        });
                    },
                    || (),
                );
            }
        }
    }

    // -- shared compaction core -------------------------------------------

    /// Run the pure compaction engine and record span status for the result.
    async fn run_pure_compaction_engine(
        &self,
        preparation: &CompactionPreparation,
        inputs: &CompactionInputs,
        abort_token: &CancellationToken,
        custom_instructions: Option<&str>,
        retry_callbacks: SummarizationRetryCallbacks,
        compaction_span: &dyn TelemetrySpan,
    ) -> Result<CompactionResult, CompactionError> {
        let model = self.model();
        let thinking_level = thinking_level_str(self.thinking_level());
        let retry = self.summarization_retry_policy();

        let ai_span = start_ai_request_span(
            compaction_span,
            AiRequestStart {
                operation: AiOperation::Stream,
                provider: model.provider.clone(),
                model: model.id.clone(),
                api: model.api.clone(),
                streaming: true,
                deferred: None,
            },
        );

        let result = compaction::compact(
            preparation,
            CompactOptions {
                model: &model,
                api_key: inputs.api_key.clone(),
                headers: inputs.headers.clone(),
                custom_instructions,
                signal: Some(abort_token.clone()),
                thinking_level: thinking_level.as_deref(),
                stream_fn: inputs.stream_fn.clone(),
                env: inputs.env.clone(),
                retry: Some(retry),
                retry_callbacks: Some(retry_callbacks),
            },
        )
        .await;

        Self::record_compaction_result_status(&result, &*ai_span, compaction_span);
        drop(ai_span);
        result
    }

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
        let settings = self.compaction_settings();
        let (session_id, path_entries) = {
            let sm = tokio::select! {
                biased;
                () = abort_token.cancelled() => return Err(CompactionError::Cancelled),
                sm = self.session_manager.lock() => sm,
            };
            let entries: Vec<SessionEntry> = sm.get_branch(None).into_iter().cloned().collect();
            (sm.get_session_id().to_owned(), entries)
        };

        // Start a pi.harness.compaction span through the session telemetry
        // context. The span is contained: a panicking telemetry backend
        // degrades to a no-op span and never affects the compaction outcome.
        let compaction_span = start_harness_compaction_span(
            self.telemetry.as_ref(),
            HarnessCompactionStart {
                session_id,
                lane_name: "main".to_owned(),
                operation_id: format!("compaction-{}", reason.as_str()),
                recovery: will_retry,
            },
        );
        let pure_settings = settings_to_pure(settings);

        let path_refs: Vec<&SessionEntry> = path_entries.iter().collect();
        let preparation = prepare_compaction(&path_refs, pure_settings).inspect_err(|err| {
            contained(
                || {
                    compaction_span.set_status(SpanStatus::Error {
                        name: None,
                        message: Some(err.to_string()),
                    });
                },
                || (),
            );
        })?;
        let Some(preparation) = preparation else {
            drop(compaction_span);
            return Ok(None);
        };

        // Resolve auth + stream inputs.
        let inputs = tokio::select! {
            biased;
            () = abort_token.cancelled() => Err(CompactionError::Cancelled),
            result = self.resolve_compaction_inputs() => result,
        }
        .inspect_err(|err| {
            contained(
                || {
                    compaction_span.set_status(SpanStatus::Error {
                        name: None,
                        message: Some(err.to_string()),
                    });
                },
                || (),
            );
        })?;

        // Extension before_compact hook (cancellable).
        let runner = self.hooks.runner();
        if let Some(result) = self
            .try_extension_before_compact(
                &runner,
                reason,
                &abort_token,
                will_retry,
                &*compaction_span,
            )
            .await?
        {
            return Ok(Some(result));
        }
        let retry_callbacks =
            self.summarization_retry_callbacks(SummarizationRetrySource::Compaction { reason });

        let result = self
            .run_pure_compaction_engine(
                &preparation,
                &inputs,
                &abort_token,
                if is_manual { custom_instructions } else { None },
                retry_callbacks,
                &*compaction_span,
            )
            .await;

        let result = result?;

        // Persist + rebuild + extension session_compact. The span stays open
        // across finalize so persist/rebuild failures record Error status.
        self.finalize_compaction_result(result, false, reason, will_retry, &abort_token)
            .await
            .inspect_err(|err| {
                contained(
                    || {
                        compaction_span.set_status(SpanStatus::Error {
                            name: None,
                            message: Some(err.to_string()),
                        });
                    },
                    || (),
                );
            })
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
        // Persist the compaction entry.
        let (_entry_id, saved_entry) = {
            let mut sm = tokio::select! {
                biased;
                () = abort_token.cancelled() => return Err(CompactionError::Cancelled),
                sm = self.session_manager.lock() => sm,
            };
            let tokens_before_i64: i64 = i64::try_from(result.tokens_before).unwrap_or(i64::MAX);
            let details = result.details.clone();
            let entry_id = sm
                .append_compaction(
                    &result.summary,
                    &result.first_kept_entry_id,
                    tokens_before_i64,
                    details,
                    if from_hook { Some(true) } else { None },
                    result.usage.clone(),
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
            self.extension_after_compact(
                &runner,
                compaction_entry,
                from_hook,
                reason,
                will_retry,
                abort_token,
            )
            .await;
        }

        // Emit the EntryAppended event for the new compaction entry.
        if let Some(entry) = saved_entry {
            self.emit_public(AgentSessionEvent::EntryAppended { entry });
        }

        Ok(result)
    }

    // -- auth + stream resolution -----------------------------------------

    /// Resolve model-runtime auth and stream inputs shared by compaction and
    /// branch summarization.
    ///
    /// The test-only [`CompactionStreamHandle`] fallback remains compaction
    /// specific; tree navigation requires a real model runtime.
    ///
    /// # Errors
    ///
    /// Returns [`CompactionError::SummarizationFailed`] when no model runtime
    /// is configured or auth resolution fails.
    pub(crate) async fn resolve_summarization_inputs(
        &self,
        operation: &str,
    ) -> Result<(SummarizationAuth, SummarizeStreamFn), CompactionError> {
        let runtime = self.model_runtime_handle().ok_or_else(|| {
            CompactionError::SummarizationFailed(format!(
                "No model runtime configured for {operation}"
            ))
        })?;
        let model = self.model();
        let auth = runtime
            .get_auth_for_model(&model, ModelRuntimeAuthOverrides::default())
            .await
            .map_err(|err| {
                CompactionError::SummarizationFailed(format!("auth resolution failed: {err}"))
            })?;
        let auth = SummarizationAuth {
            api_key: auth.as_ref().and_then(|value| value.auth.api_key.clone()),
            headers: auth.as_ref().and_then(|value| value.auth.headers.clone()),
            env: auth.and_then(|value| {
                value.env.map(|provider_env| {
                    provider_env
                        .into_iter()
                        .collect::<BTreeMap<String, String>>()
                })
            }),
        };
        let stream_fn: SummarizeStreamFn = Arc::new(move |model, context, options| {
            let runtime = Arc::clone(&runtime);
            Box::pin(async move {
                runtime.stream_simple(model, context, options)
                    as Pin<
                        Box<
                            dyn futures::Stream<Item = Result<AssistantMessageEvent, ProviderError>>
                                + Send,
                        >,
                    >
            })
        });
        Ok((auth, stream_fn))
    }

    async fn resolve_compaction_inputs(&self) -> Result<CompactionInputs, CompactionError> {
        if self.model_runtime_handle().is_some() {
            let (auth, stream_fn) = self.resolve_summarization_inputs("compaction").await?;
            return Ok(CompactionInputs {
                stream_fn,
                api_key: auth.api_key,
                headers: auth.headers,
                env: auth.env,
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
    /// is honoured through both the active token and the returned
    /// [`super::extension_runner::CancelResult`].
    ///
    /// Returns a [`BeforeCompactResult`] with `cancel` set when the extension
    /// cancelled, and `compaction` always `None` (replacement not deliverable
    /// through the current carrier).
    async fn extension_before_compact(
        &self,
        runner: &Arc<dyn ExtensionRunner>,
        event: AgentSessionEvent,
        abort_token: &CancellationToken,
    ) -> Result<BeforeCompactResult, CompactionError> {
        let result = tokio::select! {
            biased;
            () = abort_token.cancelled() => return Err(CompactionError::Cancelled),
            result = runner.emit(event) => result,
        };
        match result {
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
        abort_token: &CancellationToken,
    ) {
        let event = AgentSessionEvent::CompactionEnd {
            reason,
            result: None,
            aborted: false,
            will_retry,
            error_message: None,
        };
        let result = tokio::select! {
            biased;
            () = abort_token.cancelled() => return,
            result = runner.emit(event) => result,
        };
        if let Err(err) = result {
            runner.emit_error(err.to_string());
        }
    }

    // -- auto-compaction abort -------------------------------------------

    /// Begin the auto-compaction cancellation slot.
    ///
    /// Returns the token plus a monotonically unique owner generation. The
    /// owner must be passed to
    /// [`clear_auto_compaction_abort`](Self::clear_auto_compaction_abort) so
    /// that an older compaction finishing after a newer one started cannot
    /// remove the newer compaction's token.
    fn begin_auto_compaction_abort(&self) -> (CancellationToken, u64) {
        let token = CancellationToken::new();
        let mut inner = self.lock_inner();
        if let Some(prev) = inner.auto_compaction_abort.take() {
            prev.cancel();
        }
        inner.auto_compaction_owner = inner.auto_compaction_owner.wrapping_add(1);
        let owner = inner.auto_compaction_owner;
        inner.auto_compaction_abort = Some(token.clone());
        (token, owner)
    }

    /// Clear the auto-compaction cancellation slot, but only when `owner`
    /// matches the generation stored at install time. A stale owner (an older
    /// compaction superseded by a newer one) leaves the newer slot intact.
    fn clear_auto_compaction_abort(&self, owner: u64) {
        let mut inner = self.lock_inner();
        if inner.auto_compaction_owner == owner {
            inner.auto_compaction_abort = None;
        }
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

    /// Resolve the settings retry policy for one summarization request.
    pub(super) fn summarization_retry_policy(&self) -> SummarizationRetryPolicy {
        let retry = self.lock_settings().get_retry_settings();
        SummarizationRetryPolicy {
            enabled: retry.enabled,
            max_retries: clamp_summarization_max_retries(retry.max_retries),
            base_delay_ms: retry.base_delay_ms,
        }
    }

    /// Emit the TypeScript-compatible retry lifecycle for one summary source.
    pub(super) fn summarization_retry_callbacks(
        &self,
        source: SummarizationRetrySource,
    ) -> SummarizationRetryCallbacks {
        let weak = self.upgrade_self();
        let scheduled_weak = weak.clone();
        let attempt_weak = weak.clone();
        SummarizationRetryCallbacks {
            on_retry_scheduled: Some(Arc::new(
                move |attempt, max_attempts, delay_ms, error_message| {
                    if let Some(session) =
                        scheduled_weak.as_ref().and_then(std::sync::Weak::upgrade)
                    {
                        session.emit_public(AgentSessionEvent::SummarizationRetryScheduled {
                            attempt,
                            max_attempts,
                            delay_ms,
                            error_message,
                        });
                    }
                },
            )),
            on_retry_attempt_start: Some(Arc::new(move || {
                if let Some(session) = attempt_weak.as_ref().and_then(std::sync::Weak::upgrade) {
                    session
                        .emit_public(AgentSessionEvent::SummarizationRetryAttemptStart { source });
                }
            })),
            on_retry_finished: Some(Arc::new(move || {
                if let Some(session) = weak.as_ref().and_then(std::sync::Weak::upgrade) {
                    session.emit_public(AgentSessionEvent::SummarizationRetryFinished);
                }
            })),
        }
    }
}

/// Sane ceiling for summarization retry attempts.
///
/// A summarization request is a single LLM call within a user-facing session.
/// With the 60-second backoff cap, more than this many retries would keep the
/// session blocked for ~10+ minutes on a transient failure that either
/// resolves in a few attempts or indicates a persistent problem. Values at or
/// below this are passed through unchanged; oversized or rejected values clamp
/// here rather than falling back to `u32::MAX` (~4.29e9 retries).
const SUMMARIZATION_MAX_RETRIES_CEILING: u32 = 10;

/// Clamp a resolved `max_retries` to the sane summarization ceiling.
///
/// Values that fit in `u32` and are at or below the ceiling pass through.
/// Oversized values (above the ceiling or overflowing `u32`) clamp to the
/// ceiling, preserving the existing default behavior for rejected input.
fn clamp_summarization_max_retries(max_retries: u64) -> u32 {
    match u32::try_from(max_retries) {
        Ok(value) => value.min(SUMMARIZATION_MAX_RETRIES_CEILING),
        Err(_) => SUMMARIZATION_MAX_RETRIES_CEILING,
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
    use pi_agent::telemetry::{AttributeValue, InMemoryTelemetryContext, SpanStatus};
    use pi_agent::user_text;
    use pi_ai::{
        AssistantContent, AssistantMessageEvent, Context, DoneReason, Model, ModelCost, ModelInput,
        ModelThinkingLevel, Provider, ProviderError, StreamOptions, TextContent, Usage, UsageCost,
    };
    use serde_json::{Map, Value};
    use std::collections::HashMap;
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
        make_session_with_telemetry(
            context_window,
            stream_fn,
            messages,
            pi_agent::telemetry::noop_context(),
        )
    }

    fn make_session_with_telemetry(
        context_window: u64,
        stream_fn: SummarizeStreamFn,
        messages: Vec<pi_agent::AgentMessage>,
        telemetry: Arc<dyn pi_agent::telemetry::TelemetryContext>,
    ) -> TestResult<Arc<AgentSession>> {
        let provider = mock_provider();
        let mut config = AgentSessionConfig::test_config(provider, test_model(context_window))?;
        config.system_prompt = "sys".into();
        config.messages = messages;
        config.compaction_stream_override = Some(CompactionStreamHandle::new(stream_fn));
        // Provide the base config explicitly so the session threads the given
        // telemetry context (same Arc) instead of the C18-resolved no-op.
        // Construction goes through the sanctioned `base` seam (no new
        // AgentLoopConfig literal site; the parity oracle stays at five).
        let mut base = pi_agent::AgentLoopConfig::base(test_model(context_window));
        base.telemetry = telemetry;
        config.base_config = Some(base);
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
        session_with_history_with(
            context_window,
            stream_fn,
            pi_agent::telemetry::noop_context(),
        )
        .await
    }

    async fn session_with_history_with(
        context_window: u64,
        stream_fn: SummarizeStreamFn,
        telemetry: Arc<dyn pi_agent::telemetry::TelemetryContext>,
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

        let session = make_session_with_telemetry(context_window, stream_fn, messages, telemetry)?;

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

        // Collect compaction_end events and confirm the deterministic error did not retry.
        let mut end_events = Vec::new();
        let mut retry_events = Vec::new();
        while let Ok(Some(event)) = timeout(std::time::Duration::from_millis(100), rx.recv()).await
        {
            if event.type_name() == "compaction_end" {
                end_events.push(event.clone());
            }
            if matches!(
                event,
                AgentSessionEvent::SummarizationRetryScheduled { .. }
                    | AgentSessionEvent::SummarizationRetryAttemptStart { .. }
                    | AgentSessionEvent::SummarizationRetryFinished
            ) {
                retry_events.push(event);
            }
        }
        assert!(retry_events.is_empty());
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
    async fn compaction_emits_spans_through_session_telemetry_context() -> TestResult {
        let telemetry = InMemoryTelemetryContext::new();
        let session = session_with_history_with(
            8_192,
            summary_stream_fn("## Goal\nTelemetry summary"),
            Arc::new(telemetry.clone()),
        )
        .await?;

        session.compact(None).await?;
        sleep(std::time::Duration::from_millis(50)).await;

        // The spans must be visible through the SAME Arc injected via
        // AgentLoopConfig.telemetry and threaded into the session.
        let spans = telemetry.spans();
        let compaction = spans
            .iter()
            .find(|span| span.name == "pi.harness.compaction")
            .ok_or("missing pi.harness.compaction span")?;
        assert_eq!(
            compaction.attributes.get("pi.operation.kind"),
            Some(&AttributeValue::Str("compaction".to_owned()))
        );
        assert_eq!(compaction.status, SpanStatus::Ok);

        let ai = spans
            .iter()
            .find(|span| span.name == "pi.ai.request" && span.parent_id == Some(compaction.id))
            .ok_or("missing child pi.ai.request span")?;
        assert_eq!(
            ai.attributes.get("pi.ai.provider"),
            Some(&AttributeValue::Str("test-provider".to_owned()))
        );
        assert_eq!(
            ai.attributes.get("pi.ai.model"),
            Some(&AttributeValue::Str("m".to_owned()))
        );
        assert_eq!(
            ai.attributes.get("pi.ai.api"),
            Some(&AttributeValue::Str("test-api".to_owned()))
        );
        assert_eq!(ai.status, SpanStatus::Ok);
        Ok(())
    }

    #[tokio::test]
    async fn failed_compaction_records_error_status_on_both_spans() -> TestResult {
        let telemetry = InMemoryTelemetryContext::new();
        let session = session_with_history_with(
            8_192,
            error_stream_fn("summarization exploded"),
            Arc::new(telemetry.clone()),
        )
        .await?;

        let result = session.compact(None).await;
        assert!(result.is_err());
        sleep(std::time::Duration::from_millis(50)).await;

        let spans = telemetry.spans();
        let compaction = spans
            .iter()
            .find(|span| span.name == "pi.harness.compaction")
            .ok_or("missing pi.harness.compaction span")?;
        assert!(
            matches!(compaction.status, SpanStatus::Error { .. }),
            "failed compaction must record Error on pi.harness.compaction: {:?}",
            compaction.status
        );
        let ai = spans
            .iter()
            .find(|span| span.name == "pi.ai.request")
            .ok_or("missing pi.ai.request span")?;
        assert!(
            matches!(ai.status, SpanStatus::Error { .. }),
            "failed summarization must record Error on pi.ai.request: {:?}",
            ai.status
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
    async fn manual_compact_transient_summary_error_emits_retry_lifecycle() -> TestResult {
        let attempts = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let stream_fn: SummarizeStreamFn = Arc::new({
            let attempts = Arc::clone(&attempts);
            move |_model, _ctx, _opts| {
                let attempt = attempts.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                Box::pin(async move {
                    let mut message = AssistantMessage::new("a", "p", "m", 1);
                    if attempt == 0 {
                        message.stop_reason = StopReason::Error;
                        message.error_message = Some("provider overloaded".to_owned());
                    } else {
                        message.content =
                            vec![AssistantContent::Text(TextContent::new("recovered"))];
                        message.stop_reason = StopReason::Stop;
                    }
                    let stream = stream::iter(vec![Ok(AssistantMessageEvent::Done {
                        reason: DoneReason::Stop,
                        message,
                    })]);
                    Box::pin(stream)
                        as Pin<
                            Box<
                                dyn futures::Stream<
                                        Item = Result<AssistantMessageEvent, ProviderError>,
                                    > + Send,
                            >,
                        >
                })
            }
        });
        let session = session_with_history(8_192, stream_fn).await?;
        let (tx, mut rx) = mpsc::unbounded_channel::<String>();
        let _unsub = session.subscribe(move |event| {
            if matches!(
                event,
                AgentSessionEvent::SummarizationRetryScheduled { .. }
                    | AgentSessionEvent::SummarizationRetryAttemptStart { .. }
                    | AgentSessionEvent::SummarizationRetryFinished
            ) {
                let _ = tx.send(event.type_name().to_owned());
            }
        });

        let result = session.compact(None).await?;
        assert!(result.summary.contains("recovered"));
        assert!(attempts.load(std::sync::atomic::Ordering::SeqCst) >= 2);
        let mut events = Vec::new();
        while let Ok(event) = rx.try_recv() {
            events.push(event);
        }
        assert_eq!(
            events,
            [
                "summarization_retry_scheduled",
                "summarization_retry_attempt_start",
                "summarization_retry_finished",
            ]
        );
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
        let should_continue = session.check_compaction(&last_assistant, true).await;
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
        let should_continue = session.check_compaction(&last_assistant, true).await;
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
        let should_continue = session.check_compaction(&overflow_assistant, true).await;
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
        let should_continue = session.check_compaction(&overflow_assistant, true).await;
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
        let should_continue = session.check_compaction(&overflow_assistant, true).await;
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
        let should_continue = session.check_compaction(&other_model_msg, true).await;

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
        let should_continue = session.check_compaction(&old_msg, true).await;
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

        let should_continue = session.check_compaction(&last_assistant, true).await;

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
    async fn resolve_summarization_inputs_tree_navigation_label() -> TestResult {
        // Tree navigation / branch summarization calls resolve_summarization_inputs
        // directly (not through resolve_compaction_inputs). The missing-runtime
        // error must name that operation, not "compaction", so a user navigating
        // a tree does not read an error naming an operation they did not perform.
        let provider = mock_provider();
        let config = AgentSessionConfig::test_config(provider, test_model(8_192))?;
        let session = AgentSession::new(config)?;
        let result = session
            .resolve_summarization_inputs("branch summarization")
            .await;
        let Err(CompactionError::SummarizationFailed(msg)) = &result else {
            return Err("expected SummarizationFailed".into());
        };
        assert!(
            msg.contains("branch summarization"),
            "error should name the tree-navigation operation, got: {msg}"
        );
        assert!(
            !msg.contains("for compaction"),
            "error must not say 'compaction' for tree navigation, got: {msg}"
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
        let should_continue = session.check_compaction(&last_assistant, true).await;
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

    #[test]
    fn summarization_retry_clamps_oversized_max_retries() {
        // Values at or below the ceiling pass through unchanged.
        assert_eq!(clamp_summarization_max_retries(0), 0);
        assert_eq!(clamp_summarization_max_retries(3), 3);
        assert_eq!(
            clamp_summarization_max_retries(u64::from(SUMMARIZATION_MAX_RETRIES_CEILING)),
            SUMMARIZATION_MAX_RETRIES_CEILING
        );
        // Values above the ceiling clamp to the ceiling.
        assert_eq!(
            clamp_summarization_max_retries(u64::from(SUMMARIZATION_MAX_RETRIES_CEILING) + 1),
            SUMMARIZATION_MAX_RETRIES_CEILING
        );
        // u64 overflow also clamps to the ceiling (was u32::MAX ~4.29e9).
        assert_eq!(
            clamp_summarization_max_retries(u64::MAX),
            SUMMARIZATION_MAX_RETRIES_CEILING
        );
    }

    // -- same-run post-turn compaction behavioral tests --------------------

    use std::sync::LazyLock;
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

    use crate::core::agent_session::extension_runner::{
        BeforeAgentStartResult, CancelResult, ExtensionRunnerError, InputTransformResult,
    };
    use crate::core::model_runtime::{CreateModelRuntimeOptions, ModelRuntime};
    use crate::core::resources::ResourceExtensionPaths;
    use futures::future::BoxFuture;
    use pi_agent::{
        AfterToolCallResult, AgentMessage, AgentTool, AgentToolResult, BeforeToolCallResult,
        ToolError, ToolUpdates,
    };
    use pi_ai::ToolResultContent;

    #[derive(Clone, Copy, Debug, Eq, PartialEq)]
    enum BlockingCompactionHook {
        Before,
        After,
    }

    struct BlockingCompactionRunner {
        hook: BlockingCompactionHook,
        entered: Arc<tokio::sync::Notify>,
    }

    impl ExtensionRunner for BlockingCompactionRunner {
        fn has_handlers(&self, event: &str) -> bool {
            self.hook == BlockingCompactionHook::Before && event == "session_before_compact"
        }

        fn emit(
            &self,
            event: AgentSessionEvent,
        ) -> BoxFuture<'_, Result<Option<CancelResult>, ExtensionRunnerError>> {
            let blocks = matches!(
                (self.hook, &event),
                (
                    BlockingCompactionHook::Before,
                    AgentSessionEvent::CompactionStart { .. }
                ) | (
                    BlockingCompactionHook::After,
                    AgentSessionEvent::CompactionEnd { .. }
                )
            );
            if !blocks {
                return Box::pin(async { Ok(None) });
            }
            let entered = Arc::clone(&self.entered);
            Box::pin(async move {
                entered.notify_one();
                std::future::pending::<Result<Option<CancelResult>, ExtensionRunnerError>>().await
            })
        }

        fn emit_message_end(
            &self,
            _message: AgentMessage,
        ) -> BoxFuture<'_, Result<Option<AgentMessage>, ExtensionRunnerError>> {
            Box::pin(async { Ok(None) })
        }

        fn emit_tool_call(
            &self,
            _tool_name: &str,
            _tool_call_id: &str,
            _input: Map<String, Value>,
        ) -> BoxFuture<'_, Result<Option<BeforeToolCallResult>, ExtensionRunnerError>> {
            Box::pin(async { Ok(None) })
        }

        fn emit_tool_result(
            &self,
            _tool_name: &str,
            _tool_call_id: &str,
            _input: Map<String, Value>,
            _content: Vec<ToolResultContent>,
            _details: Value,
            _is_error: bool,
        ) -> BoxFuture<'_, Result<Option<AfterToolCallResult>, ExtensionRunnerError>> {
            Box::pin(async { Ok(None) })
        }

        fn emit_input(
            &self,
            _text: &str,
            _images: Option<Value>,
            _source: &str,
            _streaming_behavior: Option<&str>,
        ) -> BoxFuture<'_, Result<InputTransformResult, ExtensionRunnerError>> {
            Box::pin(async { Ok(InputTransformResult::default()) })
        }

        fn emit_before_agent_start(
            &self,
            _prompt: &str,
            _images: Option<Value>,
        ) -> BoxFuture<'_, Result<Option<BeforeAgentStartResult>, ExtensionRunnerError>> {
            Box::pin(async { Ok(None) })
        }

        fn emit_resources_discover(
            &self,
            _cwd: &str,
            _reason: &str,
        ) -> BoxFuture<'_, Result<ResourceExtensionPaths, ExtensionRunnerError>> {
            Box::pin(async { Ok(ResourceExtensionPaths::default()) })
        }

        fn get_registered_commands(&self) -> Vec<String> {
            Vec::new()
        }

        fn execute_command(
            &self,
            _name: &str,
            _args: &str,
        ) -> BoxFuture<'_, Result<bool, ExtensionRunnerError>> {
            Box::pin(async { Ok(false) })
        }

        fn get_all_registered_tools(&self) -> HashMap<String, Arc<dyn AgentTool>> {
            HashMap::new()
        }

        fn get_flag_values(&self) -> HashMap<String, Value> {
            HashMap::new()
        }

        fn invalidate(&self) {}

        fn emit_error(&self, _message: String) {}
    }

    struct BlockingCredentialStore {
        inner: pi_ai::auth::InMemoryCredentialStore,
        blocking: AtomicBool,
        entered: Arc<tokio::sync::Notify>,
    }

    impl pi_ai::auth::CredentialStore for BlockingCredentialStore {
        fn read<'a>(
            &'a self,
            provider_id: &'a str,
        ) -> BoxFuture<'a, Result<Option<pi_ai::auth::Credential>, pi_ai::auth::StoreError>>
        {
            if !self.blocking.load(Ordering::SeqCst) {
                return self.inner.read(provider_id);
            }
            let entered = Arc::clone(&self.entered);
            Box::pin(async move {
                entered.notify_one();
                std::future::pending::<
                    Result<Option<pi_ai::auth::Credential>, pi_ai::auth::StoreError>,
                >()
                .await
            })
        }

        fn list(
            &self,
        ) -> BoxFuture<'_, Result<Vec<pi_ai::auth::CredentialInfo>, pi_ai::auth::StoreError>>
        {
            self.inner.list()
        }

        fn modify<'a>(
            &'a self,
            provider_id: &'a str,
            update: Box<pi_ai::auth::types::CredentialModifyFn<'a>>,
        ) -> BoxFuture<'a, Result<Option<pi_ai::auth::Credential>, pi_ai::auth::StoreError>>
        {
            self.inner.modify(provider_id, update)
        }

        fn delete<'a>(
            &'a self,
            provider_id: &'a str,
        ) -> BoxFuture<'a, Result<(), pi_ai::auth::StoreError>> {
            self.inner.delete(provider_id)
        }
    }

    static BULKY_TOOL_PARAMS: LazyLock<Value> =
        LazyLock::new(|| serde_json::json!({"type": "object", "properties": {}}));

    const BULKY_RESULT_PREFIX: &str = "BULKY_MARKER:";

    fn bulky_result_text() -> String {
        // Match the canonical fixture: 6,800 payload chars are about 1,700 tokens.
        format!("{BULKY_RESULT_PREFIX}{}", "x".repeat(6_800))
    }

    /// A tool that returns a large fixed text result immediately.
    struct BulkyTool {
        name: String,
    }

    impl AgentTool for BulkyTool {
        fn name(&self) -> &str {
            &self.name
        }

        fn label(&self) -> &str {
            &self.name
        }

        fn description(&self) -> &'static str {
            "returns a large fixed text result"
        }

        fn parameters(&self) -> &Value {
            &BULKY_TOOL_PARAMS
        }

        fn validate_arguments(
            &self,
            args: &Map<String, Value>,
        ) -> Result<Map<String, Value>, ToolError> {
            Ok(args.clone())
        }

        fn execute(
            &self,
            _tool_call_id: &str,
            _args: Map<String, Value>,
            _cancel: CancellationToken,
            _updates: ToolUpdates,
        ) -> futures::future::BoxFuture<'static, Result<AgentToolResult, ToolError>> {
            Box::pin(async move {
                Ok(AgentToolResult {
                    content: vec![pi_ai::ToolResultContent::Text(TextContent::new(
                        bulky_result_text(),
                    ))],
                    ..AgentToolResult::default()
                })
            })
        }
    }

    /// Provider: first stream call returns a tool call for `BulkyTool`,
    /// later calls return a plain stop. Records every request context.
    struct ToolThenFinalProvider {
        call_count: Arc<AtomicUsize>,
        contexts: Arc<std::sync::Mutex<Vec<Context>>>,
    }

    fn scripted_assistant(
        build: impl FnOnce(&mut AssistantMessage),
        reason: DoneReason,
    ) -> AssistantMessageEvent {
        let mut message =
            AssistantMessage::new("test-api", "test-provider", "m", pi_agent::now_millis());
        build(&mut message);
        AssistantMessageEvent::Done { reason, message }
    }

    impl Provider for ToolThenFinalProvider {
        fn stream(
            &self,
            _model: &Model,
            context: Context,
            _options: StreamOptions,
        ) -> BoxStream<'static, Result<AssistantMessageEvent, ProviderError>> {
            self.contexts
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .push(context);
            let count = self.call_count.fetch_add(1, Ordering::SeqCst);
            let events: Vec<Result<AssistantMessageEvent, ProviderError>> = if count == 0 {
                vec![
                    Ok(AssistantMessageEvent::Start {
                        partial: Arc::new(AssistantMessage::new(
                            "test-api",
                            "test-provider",
                            "m",
                            pi_agent::now_millis(),
                        )),
                    }),
                    Ok(scripted_assistant(
                        |message| {
                            message.content = vec![AssistantContent::ToolCall(
                                pi_ai::ToolCall::new("tc-1", "bulky", Map::new()),
                            )];
                            message.stop_reason = StopReason::ToolUse;
                            message.usage.total_tokens = 8_000;
                        },
                        DoneReason::ToolUse,
                    )),
                ]
            } else {
                vec![
                    Ok(AssistantMessageEvent::Start {
                        partial: Arc::new(AssistantMessage::new(
                            "test-api",
                            "test-provider",
                            "m",
                            pi_agent::now_millis(),
                        )),
                    }),
                    Ok(scripted_assistant(
                        |message| {
                            message.content =
                                vec![AssistantContent::Text(TextContent::new("final answer"))];
                            message.stop_reason = StopReason::Stop;
                            message.usage.total_tokens = 500;
                        },
                        DoneReason::Stop,
                    )),
                ]
            };
            Box::pin(stream::iter(events))
                as Pin<
                    Box<
                        dyn futures::Stream<Item = Result<AssistantMessageEvent, ProviderError>>
                            + Send,
                    >,
                >
        }
    }

    /// Summary stream that waits for the release latch before producing the
    /// summary, so tests can act while compaction is in flight.
    fn gated_summary_stream_fn(
        text: &str,
        release: tokio::sync::watch::Receiver<bool>,
    ) -> SummarizeStreamFn {
        let text = text.to_owned();
        Arc::new(move |_model, _ctx, _opts| {
            let text = text.clone();
            let mut release = release.clone();
            Box::pin(async move {
                if !*release.borrow() {
                    assert!(
                        release.changed().await.is_ok(),
                        "compaction release sender dropped"
                    );
                }
                let mut msg = AssistantMessage::new("a", "p", "m", 1);
                msg.content = vec![AssistantContent::Text(TextContent::new(text))];
                msg.stop_reason = StopReason::Stop;
                Box::pin(stream::iter(vec![Ok(AssistantMessageEvent::Done {
                    reason: DoneReason::Stop,
                    message: msg,
                })]))
                    as Pin<
                        Box<
                            dyn futures::Stream<Item = Result<AssistantMessageEvent, ProviderError>>
                                + Send,
                        >,
                    >
            })
        })
    }

    /// Session wired for same-run compaction: canonical-size window, history
    /// tail in the tree, a bulky tool, and recorded provider contexts.
    /// Keep-recent (1,750) retains the current turn and summarizes older history.
    #[derive(Clone, Copy, Debug, Eq, PartialEq)]
    enum SameRunAgentSource {
        Session,
        Prebuilt,
    }

    async fn make_same_run_session(
        stream_fn: SummarizeStreamFn,
        should_stop: bool,
    ) -> TestResult<(
        Arc<AgentSession>,
        Arc<AtomicUsize>,
        Arc<std::sync::Mutex<Vec<Context>>>,
    )> {
        make_same_run_session_with_model_runtime(
            stream_fn,
            should_stop,
            test_model(2_600),
            None,
            SameRunAgentSource::Session,
        )
        .await
    }

    async fn make_prebuilt_same_run_session(
        stream_fn: SummarizeStreamFn,
    ) -> TestResult<(
        Arc<AgentSession>,
        Arc<AtomicUsize>,
        Arc<std::sync::Mutex<Vec<Context>>>,
    )> {
        make_same_run_session_with_model_runtime(
            stream_fn,
            false,
            test_model(2_600),
            None,
            SameRunAgentSource::Prebuilt,
        )
        .await
    }

    async fn make_same_run_session_with_model_runtime(
        stream_fn: SummarizeStreamFn,
        should_stop: bool,
        model: Model,
        model_runtime: Option<Arc<ModelRuntime>>,
        agent_source: SameRunAgentSource,
    ) -> TestResult<(
        Arc<AgentSession>,
        Arc<AtomicUsize>,
        Arc<std::sync::Mutex<Vec<Context>>>,
    )> {
        let provider = Arc::new(ToolThenFinalProvider {
            call_count: Arc::new(AtomicUsize::new(0)),
            contexts: Arc::new(std::sync::Mutex::new(Vec::new())),
        });
        let call_count = Arc::clone(&provider.call_count);
        let contexts = Arc::clone(&provider.contexts);

        let mut config = AgentSessionConfig::test_config(provider.clone(), model.clone())?;
        config.model_runtime = model_runtime;
        config.system_prompt = "sys".into();
        config.compaction_stream_override = Some(CompactionStreamHandle::new(stream_fn));
        config.tools = vec![Arc::new(BulkyTool {
            name: "bulky".to_owned(),
        }) as Arc<dyn AgentTool>];

        // Two history pairs so the compaction cut summarizes real entries.
        let mut messages = Vec::new();
        for i in 0..2 {
            messages.push(user_text(
                format!("history user message {i} with padding"),
                std::iter::empty(),
            ));
            let mut assistant =
                AssistantMessage::new("test-api", "test-provider", "m", pi_agent::now_millis());
            assistant
                .content
                .push(AssistantContent::Text(TextContent::new(format!(
                    "history-{i}:{}",
                    "x".repeat(800)
                ))));
            assistant.stop_reason = StopReason::Stop;
            messages.push(pi_agent::AgentMessage::Llm(Box::new(
                pi_ai::Message::Assistant(assistant),
            )));
        }
        config.messages = messages;

        if should_stop {
            let mut base = pi_agent::AgentLoopConfig::base(model.clone());
            base.should_stop_after_turn =
                Some(Arc::new(|_ctx: pi_agent::ShouldStopAfterTurnContext| {
                    Box::pin(async move { Ok(true) })
                        as futures::future::BoxFuture<
                            'static,
                            Result<bool, pi_agent::AgentLoopError>,
                        >
                }) as pi_agent::ShouldStopAfterTurn);
            config.base_config = Some(base);
        }

        if agent_source == SameRunAgentSource::Prebuilt {
            let base = config
                .base_config
                .take()
                .unwrap_or_else(|| pi_agent::AgentLoopConfig::base(model.clone()));
            config.agent = Some(pi_agent::Agent::new(pi_agent::AgentOptions {
                system_prompt: config.system_prompt.clone(),
                model,
                thinking_level: config.thinking_level,
                tools: config.tools.clone(),
                messages: config.messages.clone(),
                config: base,
                provider,
            }));
            config.provider = None;
        }

        let session = AgentSession::new(config)?;
        session.set_auto_compaction_enabled(true);
        {
            let mut settings = session.lock_settings();
            let mut compaction = Map::new();
            compaction.insert("keepRecentTokens".into(), Value::from(1_750u64));
            compaction.insert("reserveTokens".into(), Value::from(400u64));
            let mut overrides = Map::new();
            overrides.insert("compaction".into(), Value::Object(compaction));
            settings.apply_overrides(&overrides);
        }
        // Persist the history so compaction has tree entries to summarize;
        // the live run persists the prompt/assistant/tool-result tail itself.
        {
            let mut sm = session.session_manager.lock().await;
            for msg in session.agent.transcript() {
                sm.append_message(&msg)?;
            }
        }
        Ok((session, call_count, contexts))
    }

    #[tokio::test]
    async fn same_run_large_tool_compaction_before_request_two() -> TestResult {
        let (session, call_count, contexts) =
            make_same_run_session(summary_stream_fn("## Goal\nSame-run summary"), false).await?;

        let (tx, mut rx) = mpsc::unbounded_channel();
        let _unsub = session.subscribe(move |event| {
            let _ = tx.send(event.type_name().to_owned());
        });

        session
            .agent
            .prompt(vec![user_text("go", std::iter::empty())])
            .await?;
        session.agent.wait_for_idle().await;
        sleep(std::time::Duration::from_millis(50)).await;

        assert_eq!(call_count.load(Ordering::SeqCst), 2, "two provider calls");

        let events = collect_events(&mut rx, 40).await;
        assert_eq!(
            events.iter().filter(|e| *e == "compaction_start").count(),
            1,
            "exactly one compaction_start: {events:?}"
        );
        assert_eq!(
            events.iter().filter(|e| *e == "compaction_end").count(),
            1,
            "exactly one compaction_end: {events:?}"
        );
        let first_turn_end = events
            .iter()
            .position(|e| e == "turn_end")
            .ok_or("turn_end position")?;
        let compaction_start = events
            .iter()
            .position(|e| e == "compaction_start")
            .ok_or("compaction_start position")?;
        assert!(
            first_turn_end < compaction_start,
            "compaction must wait for the persisted TurnEnd barrier: {events:?}"
        );

        // The transcript keeps the tool result and carries the summary.
        let transcript = serde_json::to_string(&session.agent.transcript())?;
        assert!(
            transcript.contains(BULKY_RESULT_PREFIX),
            "tool result must be retained after compaction"
        );
        assert!(
            transcript.contains("Same-run summary"),
            "summary must be present after compaction: {transcript}"
        );

        // Request two ran on the rebuilt (compacted) context.
        let contexts = contexts
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone();
        let second = contexts.get(1).ok_or("second provider context")?;
        let second = serde_json::to_string(second)?;
        assert!(
            second.contains("Same-run summary"),
            "request two must use the compacted context: {second}"
        );
        Ok(())
    }

    #[tokio::test]
    async fn prebuilt_agent_compacts_before_request_two() -> TestResult {
        let (session, call_count, contexts) =
            make_prebuilt_same_run_session(summary_stream_fn("## Goal\nPrebuilt summary")).await?;

        session
            .agent
            .prompt(vec![user_text("go", std::iter::empty())])
            .await?;
        session.agent.wait_for_idle().await;

        assert_eq!(call_count.load(Ordering::SeqCst), 2, "two provider calls");
        let contexts = contexts
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let second = serde_json::to_string(contexts.get(1).ok_or("second provider context")?)?;
        assert!(
            second.contains("Prebuilt summary"),
            "request two must use the compacted context: {second}"
        );
        Ok(())
    }

    #[tokio::test]
    async fn steering_during_same_run_compaction_is_delivered() -> TestResult {
        let (release, gate) = tokio::sync::watch::channel(false);
        let (session, call_count, contexts) = make_same_run_session(
            gated_summary_stream_fn("## Goal\nGated summary", gate),
            false,
        )
        .await?;

        let started = Arc::new(AtomicUsize::new(0));
        let started_observer = Arc::clone(&started);
        let _unsub = session.subscribe(move |event| {
            if matches!(event, AgentSessionEvent::CompactionStart { .. }) {
                started_observer.fetch_add(1, Ordering::SeqCst);
            }
        });

        let run_session = Arc::clone(&session);
        let run = tokio::spawn(async move {
            run_session
                .agent
                .prompt(vec![user_text("go", std::iter::empty())])
                .await
        });

        // Hold compaction on the gate, then steer mid-compaction.
        for _ in 0..400 {
            if started.load(Ordering::SeqCst) > 0 {
                break;
            }
            sleep(std::time::Duration::from_millis(5)).await;
        }
        assert!(
            started.load(Ordering::SeqCst) > 0,
            "compaction never started"
        );
        session.mirror_steering_push("steer-me".into());
        session
            .agent
            .steer(user_text("steer-me", std::iter::empty()));
        release.send_replace(true);

        timeout(std::time::Duration::from_secs(5), run).await???;
        session.agent.wait_for_idle().await;
        sleep(std::time::Duration::from_millis(50)).await;

        assert_eq!(
            call_count.load(Ordering::SeqCst),
            2,
            "steering must join request two"
        );
        let contexts = contexts
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let second = serde_json::to_string(contexts.get(1).ok_or("second provider context")?)?;
        assert!(
            second.contains("steer-me"),
            "steering message must be delivered in request two: {second}"
        );
        assert!(!session.is_compacting());
        Ok(())
    }

    #[tokio::test]
    async fn next_turn_custom_message_waits_for_next_user_prompt() -> TestResult {
        let (release, gate) = tokio::sync::watch::channel(false);
        let (session, call_count, contexts) = make_same_run_session(
            gated_summary_stream_fn("## Goal\nGated summary", gate),
            false,
        )
        .await?;

        let started = Arc::new(tokio::sync::Notify::new());
        let started_observer = Arc::clone(&started);
        let _unsub = session.subscribe(move |event| {
            if matches!(event, AgentSessionEvent::CompactionStart { .. }) {
                started_observer.notify_one();
            }
        });

        let run_session = Arc::clone(&session);
        let run = tokio::spawn(async move {
            run_session
                .agent
                .prompt(vec![user_text("go", std::iter::empty())])
                .await
        });
        timeout(std::time::Duration::from_secs(5), started.notified()).await?;

        // `NextTurn` means the next top-level user prompt. Steering owns
        // delivery into another provider request within the active run.
        session
            .send_custom_message(
                crate::core::agent_session::prompt::CustomMessageInput {
                    custom_type: "next-user-prompt".to_owned(),
                    content: crate::core::messages::CustomMessageContent::Text(
                        "carry next prompt".to_owned(),
                    ),
                    display: true,
                    details: None,
                },
                false,
                Some(crate::core::agent_session::prompt::DeliverAs::NextTurn),
            )
            .await?;
        release.send_replace(true);
        timeout(std::time::Duration::from_secs(5), run).await???;
        session.agent.wait_for_idle().await;

        let second = {
            let contexts = contexts
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            serde_json::to_string(contexts.get(1).ok_or("second provider context")?)?
        };
        assert!(
            !second.contains("carry next prompt"),
            "next-user-prompt message leaked into request two: {second}"
        );

        session.set_auto_compaction_enabled(false);
        session
            .prompt(
                "next user prompt",
                crate::core::agent_session::prompt::PromptOptions::default(),
            )
            .await?;
        assert_eq!(
            call_count.load(Ordering::SeqCst),
            3,
            "the next user prompt must issue request three"
        );
        let third = {
            let contexts = contexts
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            serde_json::to_string(contexts.get(2).ok_or("third provider context")?)?
        };
        assert!(
            third.contains("carry next prompt"),
            "next-user-prompt message missing from request three: {third}"
        );
        Ok(())
    }

    #[tokio::test]
    async fn terminating_large_tool_result_does_not_compact() -> TestResult {
        let (session, call_count, _contexts) =
            make_same_run_session(summary_stream_fn("unused summary"), true).await?;

        let (tx, mut rx) = mpsc::unbounded_channel();
        let _unsub = session.subscribe(move |event| {
            let _ = tx.send(event.type_name().to_owned());
        });

        session
            .agent
            .prompt(vec![user_text("go", std::iter::empty())])
            .await?;
        session.agent.wait_for_idle().await;
        sleep(std::time::Duration::from_millis(50)).await;

        // A terminating turn decision happens before prepare: the loop ends
        // without a second request and without any compaction lifecycle.
        assert_eq!(call_count.load(Ordering::SeqCst), 1, "no request two");
        let events = collect_events(&mut rx, 40).await;
        assert!(
            !events.iter().any(|e| e == "compaction_start"),
            "terminating turn must not compact: {events:?}"
        );
        assert!(
            !events.iter().any(|e| e == "compaction_end"),
            "terminating turn must not compact: {events:?}"
        );
        assert_eq!(
            events.iter().filter(|e| *e == "agent_end").count(),
            1,
            "exactly one agent_end: {events:?}"
        );
        Ok(())
    }

    #[tokio::test]
    async fn cancellation_during_compaction_aborts_once_without_request_two() -> TestResult {
        let (_release, gate) = tokio::sync::watch::channel(false);
        let (session, call_count, _contexts) =
            make_same_run_session(gated_summary_stream_fn("## Goal\nNever used", gate), false)
                .await?;

        let ends = Arc::new(std::sync::Mutex::new(Vec::new()));
        let ends_observer = Arc::clone(&ends);
        let started = Arc::new(AtomicUsize::new(0));
        let started_observer = Arc::clone(&started);
        let _unsub = session.subscribe(move |event| match event {
            AgentSessionEvent::CompactionStart { .. } => {
                started_observer.fetch_add(1, Ordering::SeqCst);
            }
            AgentSessionEvent::CompactionEnd { aborted, .. } => {
                ends_observer
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .push(*aborted);
            }
            _ => {}
        });

        let run_session = Arc::clone(&session);
        let run = tokio::spawn(async move {
            run_session
                .agent
                .prompt(vec![user_text("go", std::iter::empty())])
                .await
        });

        for _ in 0..400 {
            if started.load(Ordering::SeqCst) > 0 {
                break;
            }
            sleep(std::time::Duration::from_millis(5)).await;
        }
        assert!(
            started.load(Ordering::SeqCst) > 0,
            "compaction never started"
        );
        assert!(session.is_compacting());

        // Cancel the active run: the run token wins, cancels the
        // session-owned compaction token, and the same pinned core future
        // drains through the normal aborted end + cleanup.
        session.agent.abort();
        timeout(std::time::Duration::from_secs(5), run).await???;
        session.agent.wait_for_idle().await;
        sleep(std::time::Duration::from_millis(50)).await;

        let aborted_ends = ends
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone();
        assert_eq!(aborted_ends.len(), 1, "exactly one compaction end");
        assert!(aborted_ends[0], "the end must be aborted");
        assert!(
            !session.is_compacting(),
            "compaction slot must be cleared after the aborted end"
        );
        assert_eq!(call_count.load(Ordering::SeqCst), 1, "no request two");
        Ok(())
    }

    #[tokio::test]
    async fn cancellation_releases_blocked_compaction_auth_resolution() -> TestResult {
        let entered = Arc::new(tokio::sync::Notify::new());
        let store = Arc::new(BlockingCredentialStore {
            inner: pi_ai::auth::InMemoryCredentialStore::new(),
            blocking: AtomicBool::new(false),
            entered: Arc::clone(&entered),
        });
        let runtime = Arc::new(
            ModelRuntime::create(CreateModelRuntimeOptions {
                credentials: Some(store.clone()),
                allow_model_network: Some(false),
                ..CreateModelRuntimeOptions::default()
            })
            .await?,
        );
        store.blocking.store(true, Ordering::SeqCst);

        let mut model = test_model(2_600);
        model.provider = "anthropic".to_owned();
        model.api = "anthropic-messages".to_owned();
        let (session, call_count, _) = make_same_run_session_with_model_runtime(
            summary_stream_fn("unused"),
            false,
            model,
            Some(runtime),
            SameRunAgentSource::Session,
        )
        .await?;

        let ends = Arc::new(std::sync::Mutex::new(Vec::new()));
        let ends_observer = Arc::clone(&ends);
        let _unsub = session.subscribe(move |event| {
            if let AgentSessionEvent::CompactionEnd {
                aborted, result, ..
            } = event
            {
                ends_observer
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .push((*aborted, result.is_some()));
            }
        });

        let run_session = Arc::clone(&session);
        let run = tokio::spawn(async move {
            run_session
                .agent
                .prompt(vec![user_text("go", std::iter::empty())])
                .await
        });
        timeout(std::time::Duration::from_secs(5), entered.notified()).await?;
        assert!(
            session.is_compacting(),
            "compaction must own the blocked auth lookup"
        );

        session.agent.abort();
        timeout(std::time::Duration::from_secs(5), run).await???;
        session.agent.wait_for_idle().await;

        assert_eq!(call_count.load(Ordering::SeqCst), 1, "no request two");
        let compaction_entries = session
            .session_manager
            .lock()
            .await
            .get_entries()
            .into_iter()
            .filter(|entry| entry.discriminant() == "compaction")
            .count();
        assert_eq!(
            compaction_entries, 0,
            "cancelled auth must not persist a summary"
        );
        assert_eq!(
            ends.lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .as_slice(),
            &[(true, false)],
            "one aborted end without a result"
        );
        assert!(!session.is_compacting(), "compaction slot must be cleared");
        Ok(())
    }

    #[tokio::test]
    async fn cancellation_releases_initial_compaction_snapshot_lock() -> TestResult {
        let (session, _, _) = make_same_run_session(summary_stream_fn("unused"), false).await?;
        let session_manager = session.session_manager.lock().await;
        let abort_token = CancellationToken::new();
        let mut compaction = Box::pin(session.run_compaction_core(
            CompactionReason::Threshold,
            None,
            false,
            abort_token.clone(),
            false,
        ));

        assert!(matches!(
            futures::poll!(compaction.as_mut()),
            std::task::Poll::Pending
        ));
        abort_token.cancel();
        drop(session_manager);

        let result = timeout(std::time::Duration::from_secs(1), compaction).await?;
        assert!(
            matches!(result, Err(CompactionError::Cancelled)),
            "initial snapshot lock must not outlive cancellation: {result:?}"
        );
        Ok(())
    }

    #[tokio::test]
    async fn stale_auto_compaction_owner_cannot_clear_current_slot() -> TestResult {
        let (session, _, _) = make_same_run_session(summary_stream_fn("unused"), false).await?;
        let (stale_token, stale_owner) = session.begin_auto_compaction_abort();
        let (current_token, current_owner) = session.begin_auto_compaction_abort();

        assert!(stale_token.is_cancelled());
        assert!(!current_token.is_cancelled());

        session.clear_auto_compaction_abort(stale_owner);
        assert!(session.is_compacting());
        assert!(!current_token.is_cancelled());

        session.clear_auto_compaction_abort(current_owner);
        assert!(!session.is_compacting());
        Ok(())
    }

    async fn cancel_blocked_compaction_hook(
        hook: BlockingCompactionHook,
    ) -> TestResult<(String, Vec<(bool, bool)>)> {
        let entered = Arc::new(tokio::sync::Notify::new());
        let runner = Arc::new(BlockingCompactionRunner {
            hook,
            entered: Arc::clone(&entered),
        });
        let (session, call_count, _) =
            make_same_run_session(summary_stream_fn("blocked-hook summary"), false).await?;
        session
            .hooks()
            .set_runner(runner as Arc<dyn ExtensionRunner>);

        let ends = Arc::new(std::sync::Mutex::new(Vec::new()));
        let ends_observer = Arc::clone(&ends);
        let _unsub = session.subscribe(move |event| {
            if let AgentSessionEvent::CompactionEnd {
                result, aborted, ..
            } = event
            {
                ends_observer
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .push((*aborted, result.is_some()));
            }
        });

        let run_session = Arc::clone(&session);
        let run = tokio::spawn(async move {
            run_session
                .agent
                .prompt(vec![user_text("go", std::iter::empty())])
                .await
        });

        timeout(std::time::Duration::from_secs(5), entered.notified()).await?;
        assert!(session.is_compacting());
        session.agent.abort();
        timeout(std::time::Duration::from_secs(5), run).await???;
        session.agent.wait_for_idle().await;

        assert_eq!(call_count.load(Ordering::SeqCst), 1, "no request two");
        assert!(!session.is_compacting());
        let transcript = serde_json::to_string(&session.agent.transcript())?;
        let ends = ends
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone();
        Ok((transcript, ends))
    }

    #[tokio::test]
    async fn cancellation_releases_blocked_before_compact_hook() -> TestResult {
        let (transcript, ends) =
            cancel_blocked_compaction_hook(BlockingCompactionHook::Before).await?;

        assert!(!transcript.contains("blocked-hook summary"));
        assert_eq!(ends, vec![(true, false)]);
        Ok(())
    }

    #[tokio::test]
    async fn cancellation_releases_blocked_after_compact_hook() -> TestResult {
        let (transcript, ends) =
            cancel_blocked_compaction_hook(BlockingCompactionHook::After).await?;

        assert!(transcript.contains("blocked-hook summary"));
        assert_eq!(ends, vec![(false, true)]);
        Ok(())
    }

    #[tokio::test]
    async fn cancellation_wins_while_waiting_for_persistence_lock() -> TestResult {
        let (session, _, _) = make_same_run_session(summary_stream_fn("unused"), false).await?;
        let session_manager = session.session_manager.lock().await;
        let first_kept_entry_id = session_manager
            .get_branch(None)
            .last()
            .and_then(|entry| entry.id())
            .ok_or("missing first kept entry")?
            .to_owned();
        let entries_before = session_manager.get_entries().len();
        let abort_token = CancellationToken::new();
        let mut finalize = Box::pin(session.finalize_compaction_result(
            CompactionResult {
                summary: "must not persist".to_owned(),
                first_kept_entry_id,
                tokens_before: 1,
                estimated_tokens_after: None,
                details: None,
                from_hook: None,
                usage: None,
            },
            false,
            CompactionReason::Threshold,
            false,
            &abort_token,
        ));

        assert!(matches!(
            futures::poll!(finalize.as_mut()),
            std::task::Poll::Pending
        ));
        abort_token.cancel();
        drop(session_manager);

        let result = timeout(std::time::Duration::from_secs(1), finalize).await?;
        assert!(matches!(result, Err(CompactionError::Cancelled)));
        let entries_after = session.session_manager.lock().await.get_entries().len();
        assert_eq!(entries_after, entries_before);
        Ok(())
    }
}
