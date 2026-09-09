//! Harness owner, constructor, configuration, and lane registry.

use std::collections::BTreeMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use futures::future::{BoxFuture, FutureExt};
use tokio::sync::{Mutex as AsyncMutex, RwLock};

use super::lane::LaneRuntime;
use super::restore::{RestoredLane, restore_session};
use super::support::{
    RuntimeConfig, captured_configuration, default_provider_conversion, ensure_lane_name,
    map_session_error,
};
use crate::context::Context;
use crate::harness::api::{
    AcquireLaneOptions, AgentHarness, AgentHarnessBuilder, AgentHarnessOptions, HarnessResources,
};
use crate::harness::bus::{HarnessEventBus, SnapshotCapture, WatchHandle};
use crate::harness::event::{
    ConfigUpdateChange, HarnessEvent, HarnessEventPayload, ValueUpdateChange,
};
use crate::harness::hooks::HookRegistry;
use crate::harness::result::{HarnessError, HarnessFault, LaneInfo, OpenOperation};
use crate::session::traits::{Session, SessionReaderExt};
use crate::session::{
    CompactionSettings, HarnessRetryPolicy, HarnessStreamOptions, LaneName, LaneState,
};
/// Runtime implementation attached to one durable session.
pub(crate) struct HarnessRuntime {
    pub(crate) session: Arc<dyn Session>,
    pub(crate) models: Arc<dyn crate::harness::api::HarnessModels>,
    pub(crate) hooks: HookRegistry,
    pub(crate) events: HarnessEventBus,
    pub(crate) config: Arc<RwLock<RuntimeConfig>>,
    pub(crate) lanes: Arc<AsyncMutex<BTreeMap<LaneName, Arc<LaneRuntime>>>>,
    pub(crate) closed: Arc<AtomicBool>,
    pub(crate) fault: Arc<Mutex<Option<Arc<HarnessFault>>>>,
}

impl HarnessRuntime {
    pub(crate) async fn create(
        options: AgentHarnessOptions,
        cx: &Context,
    ) -> Result<(Arc<dyn AgentHarness>, Vec<OpenOperation>), HarnessError> {
        let retry = options.retry.unwrap_or_default();
        retry
            .validate()
            .map_err(|_| HarnessError::InvalidRetryPolicy {
                max_retries: retry.max_retries,
                base_delay_ms: retry.base_delay_ms,
                message: "retry policy cannot be represented safely".to_owned(),
            })?;
        let compaction = options.compaction.unwrap_or_default();
        validate_tools(&options.tools)?;
        validate_active_tools(options.active_tool_names.as_deref(), &options.tools)?;
        let active_tool_names = options.active_tool_names.unwrap_or_else(|| {
            options
                .tools
                .iter()
                .map(|tool| tool.name().to_owned())
                .collect()
        });
        let model = options.model;
        let config = RuntimeConfig {
            model,
            thinking_level: options
                .thinking_level
                .unwrap_or(pi_ai::ModelThinkingLevel::Off),
            active_tool_names,
            tools: options.tools,
            resources: options.resources,
            stream_options: options.stream_options,
            retry,
            compaction,
            steering_mode: options
                .steering_mode
                .unwrap_or(crate::queue::QueueMode::All),
            follow_up_mode: options
                .follow_up_mode
                .unwrap_or(crate::queue::QueueMode::All),
            tool_execution: options.tool_execution,
            tool_context: options.tool_context,
            system_prompt: options.system_prompt,
            to_provider_messages: options
                .to_provider_messages
                .unwrap_or_else(default_provider_conversion),
            entry_projectors: options.entry_projectors,
        };

        let events = HarnessEventBus::new();
        let reporter_events = events.clone();
        let hooks = HookRegistry::new(move |event, context| reporter_events.emit(event, &context));
        let (restored, open) = restore_session(&options.session, cx)
            .await
            .map_err(map_session_error)?;
        let runtime = Arc::new(Self {
            session: Arc::clone(&options.session),
            models: Arc::clone(&options.models),
            hooks,
            events,
            config: Arc::new(RwLock::new(config)),
            lanes: Arc::new(AsyncMutex::new(BTreeMap::new())),
            closed: Arc::new(AtomicBool::new(false)),
            fault: Arc::new(Mutex::new(None)),
        });
        {
            let mut lanes = runtime.lanes.lock().await;
            for RestoredLane { name, data } in restored {
                let lane = LaneRuntime::new(Arc::clone(&runtime), name.clone(), data);
                lanes.insert(name, lane);
            }
        }
        let harness: Arc<dyn AgentHarness> = runtime;
        Ok((harness, open))
    }

    pub(crate) fn is_closed(&self) -> bool {
        self.closed.load(Ordering::Acquire)
    }

    pub(crate) fn closed_error(&self) -> HarnessError {
        if let Ok(fault) = self.fault.lock()
            && let Some(fault) = fault.as_ref()
        {
            return HarnessError::Closed {
                message: fault.message.clone(),
            };
        }
        HarnessError::Closed {
            message: "harness is closed".to_owned(),
        }
    }

    pub(crate) async fn config_snapshot(&self) -> RuntimeConfig {
        self.config.read().await.clone()
    }

    pub(crate) async fn lane_snapshot(&self) -> Vec<Arc<LaneRuntime>> {
        self.lanes.lock().await.values().cloned().collect()
    }

    pub(crate) async fn emit(&self, event: HarnessEvent, cx: &Context) {
        self.events.emit(event, cx).await;
    }

    pub(crate) async fn fault(&self, fault: HarnessFault, cx: &Context) {
        let fault = Arc::new(fault);
        {
            let mut slot = self
                .fault
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            if slot.is_some() {
                return;
            }
            *slot = Some(Arc::clone(&fault));
        }
        if let Ok(event) = HarnessEvent::new(
            None,
            false,
            HarnessEventPayload::Fault {
                code: "harness_fault".to_owned(),
                message: fault.message.clone(),
            },
        ) {
            self.events.emit(event, cx).await;
        }
        self.closed.store(true, Ordering::Release);
        self.hooks.close(Arc::clone(&fault));
        self.events.close(Arc::clone(&fault));
        let lanes = self.lane_snapshot().await;
        for lane in lanes {
            lane.seal(Arc::clone(&fault)).await;
        }
    }
}

impl AgentHarnessBuilder {
    /// Creates a durable harness and restores every open operation.
    ///
    /// # Errors
    ///
    /// Returns `HarnessError` when the retry policy or tool configuration is
    /// invalid, or when the session fails to restore.
    pub async fn create(
        options: AgentHarnessOptions,
        cx: &Context,
    ) -> Result<(Arc<dyn AgentHarness>, Vec<OpenOperation>), HarnessError> {
        HarnessRuntime::create(options, cx).await
    }
}

impl AgentHarness for HarnessRuntime {
    fn lane<'a>(
        &'a self,
        name: &'a LaneName,
        options: AcquireLaneOptions,
        cx: &'a Context,
    ) -> BoxFuture<'a, Result<Arc<dyn crate::harness::api::AgentLane>, HarnessError>> {
        Box::pin(async move {
            if self.is_closed() {
                return Err(self.closed_error());
            }
            ensure_lane_name(name)?;
            let mut lanes = self.lanes.lock().await;
            if let Some(lane) = lanes.get(name) {
                return Ok(Arc::clone(lane) as Arc<dyn crate::harness::api::AgentLane>);
            }
            let parent = match options.create_at {
                Some(Some(target)) => {
                    let existing = self
                        .session
                        .get_entry(&target, cx)
                        .await
                        .map_err(map_session_error)?;
                    if existing.is_none() {
                        return Err(HarnessError::UnknownTarget {
                            target_id: target,
                            message: "lane creation target does not exist".to_owned(),
                        });
                    }
                    Some(target)
                }
                None | Some(None) => None,
            };
            let _branch = self
                .session
                .create_branch(name, parent.as_ref(), cx)
                .await
                .map_err(map_session_error)?;
            let config = self.config_snapshot().await;
            let lane_config = captured_configuration(&config);
            let lane_state = LaneState::default();
            let tip = parent.clone();
            let mutator = self
                .session
                .begin_mutation(cx)
                .await
                .map_err(map_session_error)?;
            let writes = vec![
                super::support::set_json(&super::support::lane_config_address(name), &lane_config)
                    .map_err(map_session_error)?,
                super::support::set_json(&super::support::lane_state_address(name), &lane_state)
                    .map_err(map_session_error)?,
                super::support::set_json(&super::support::lane_branch_tip_address(name), &tip)
                    .map_err(map_session_error)?,
            ];
            // A failed metadata commit leaves a durable branch with no lane
            // metadata and there is no delete_branch to undo it: the name can
            // never be retried or restored. Seal the harness against one
            // shared fault, like commit_admission_writes. The registry guard
            // is released first because fault broadcast re-locks it through
            // lane_snapshot.
            if let Err(error) = mutator.commit(writes, cx).await {
                drop(lanes);
                self.fault(
                    HarnessFault {
                        message: format!("session operation failed: {error}"),
                        cause: Box::new(error),
                    },
                    cx,
                )
                .await;
                let stored = self.fault.lock().ok().and_then(|slot| slot.clone());
                return Err(match stored {
                    Some(fault) => super::support::sealed_rejection(&fault),
                    None => self.closed_error(),
                });
            }
            let data = super::support::LaneData::new(lane_config, tip, lane_state);
            let lane = LaneRuntime::new(Arc::new(self.clone_ref()), name.clone(), data);
            lanes.insert(name.clone(), Arc::clone(&lane));
            let event = HarnessEvent::lane(
                name.clone(),
                HarnessEventPayload::LaneCreated { at: parent },
            );
            // emit waits for listener delivery on the serialized drain worker;
            // the registry guard must be released first so a LaneCreated
            // listener can call back into `lanes()` without deadlocking.
            drop(lanes);
            self.events.emit(event, cx).await;
            Ok(lane as Arc<dyn crate::harness::api::AgentLane>)
        })
    }

    fn lanes<'a>(&'a self, cx: &'a Context) -> BoxFuture<'a, Result<Vec<LaneInfo>, HarnessError>> {
        Box::pin(async move {
            if self.is_closed() {
                return Err(self.closed_error());
            }
            // Snapshot the registry and release it before reading lane data:
            // a fault broadcast re-enters the registry through lane_snapshot
            // while the faulting commit path may still hold that lane's data
            // guard, so holding the registry across info() could deadlock.
            let lanes = self.lane_snapshot().await;
            let mut result = Vec::with_capacity(lanes.len());
            for lane in &lanes {
                result.push(lane.info(cx).await?);
            }
            Ok(result)
        })
    }

    fn get_name<'a>(
        &'a self,
        cx: &'a Context,
    ) -> BoxFuture<'a, Result<Option<String>, HarnessError>> {
        Box::pin(async move {
            if self.is_closed() {
                return Err(self.closed_error());
            }
            self.session.get_name(cx).await.map_err(map_session_error)
        })
    }

    fn set_name<'a>(
        &'a self,
        name: Option<&'a str>,
        cx: &'a Context,
    ) -> BoxFuture<'a, Result<(), HarnessError>> {
        Box::pin(async move {
            if self.is_closed() {
                return Err(self.closed_error());
            }
            self.session
                .set_name(name, cx)
                .await
                .map_err(map_session_error)?;
            self.events
                .emit(
                    HarnessEvent::global(HarnessEventPayload::ValueUpdate {
                        change: ValueUpdateChange::SessionName {
                            name: name.map(str::to_owned),
                        },
                    }),
                    cx,
                )
                .await;
            Ok(())
        })
    }

    fn get_label<'a>(
        &'a self,
        target: &'a crate::session::EntryId,
        cx: &'a Context,
    ) -> BoxFuture<'a, Result<Option<String>, HarnessError>> {
        Box::pin(async move {
            if self.is_closed() {
                return Err(self.closed_error());
            }
            self.session
                .get_label(target, cx)
                .await
                .map_err(map_session_error)
        })
    }

    fn set_label<'a>(
        &'a self,
        target: &'a crate::session::EntryId,
        label: Option<&'a str>,
        cx: &'a Context,
    ) -> BoxFuture<'a, Result<(), HarnessError>> {
        Box::pin(async move {
            if self.is_closed() {
                return Err(self.closed_error());
            }
            self.session
                .set_label(target, label, cx)
                .await
                .map_err(map_session_error)?;
            self.events
                .emit(
                    HarnessEvent::global(HarnessEventPayload::ValueUpdate {
                        change: ValueUpdateChange::EntryLabel {
                            target_id: target.clone(),
                            label: label.map(str::to_owned),
                        },
                    }),
                    cx,
                )
                .await;
            Ok(())
        })
    }

    fn get_tools<'a>(
        &'a self,
        _cx: &'a Context,
    ) -> BoxFuture<'a, Result<Vec<Arc<dyn crate::harness::tool::HarnessTool>>, HarnessError>> {
        Box::pin(async move { Ok(self.config.read().await.tools.clone()) })
    }

    fn set_tools<'a>(
        &'a self,
        tools: Vec<Arc<dyn crate::harness::tool::HarnessTool>>,
        cx: &'a Context,
    ) -> BoxFuture<'a, Result<(), HarnessError>> {
        Box::pin(async move {
            if self.is_closed() {
                return Err(self.closed_error());
            }
            validate_tools(&tools)?;
            // Reject removals still referenced by a lane's persisted
            // active_tool_names; a swapped-out name fails closed at
            // generation. The lane registry and config write locks are held
            // across check-and-swap: `lane()` takes `lanes` before reading
            // config and lane configuration commits hold a config read guard.
            // Lane metadata is read through the session, which never waits
            // on harness locks or lane data.
            let lanes = self.lanes.lock().await;
            let mut config = self.config.write().await;
            let offered: std::collections::BTreeSet<&str> =
                tools.iter().map(|tool| tool.name()).collect();
            for name in lanes.keys() {
                let stored = self
                    .session
                    .get_value(&super::support::lane_config_address(name), cx)
                    .await
                    .map_err(map_session_error)?;
                let active = stored.map_or_else(Vec::new, |stored| stored.value.active_tool_names);
                if let Some(in_use) = active
                    .iter()
                    .find(|active| !offered.contains(active.as_str()))
                {
                    return Err(HarnessError::InvalidLane {
                        lane: name.clone(),
                        reason: "tool_in_use".to_owned(),
                        message: format!(
                            "tool \"{in_use}\" is still active on lane \"{name}\" and cannot be removed"
                        ),
                    });
                }
            }
            config
                .active_tool_names
                .retain(|name| offered.contains(name.as_str()));
            config.tools = tools;
            drop(config);
            drop(lanes);
            self.events
                .emit(
                    HarnessEvent::global(HarnessEventPayload::ConfigUpdate {
                        change: ConfigUpdateChange::Tools,
                    }),
                    cx,
                )
                .await;
            Ok(())
        })
    }

    fn get_resources<'a>(
        &'a self,
        _cx: &'a Context,
    ) -> BoxFuture<'a, Result<HarnessResources, HarnessError>> {
        Box::pin(async move { Ok(self.config.read().await.resources.clone()) })
    }

    fn set_resources<'a>(
        &'a self,
        resources: HarnessResources,
        cx: &'a Context,
    ) -> BoxFuture<'a, Result<(), HarnessError>> {
        Box::pin(async move {
            if self.is_closed() {
                return Err(self.closed_error());
            }
            self.config.write().await.resources = resources;
            self.events
                .emit(
                    HarnessEvent::global(HarnessEventPayload::ConfigUpdate {
                        change: ConfigUpdateChange::Resources,
                    }),
                    cx,
                )
                .await;
            Ok(())
        })
    }

    fn get_stream_options<'a>(
        &'a self,
        _cx: &'a Context,
    ) -> BoxFuture<'a, Result<HarnessStreamOptions, HarnessError>> {
        Box::pin(async move { Ok(self.config.read().await.stream_options.clone()) })
    }

    fn set_stream_options<'a>(
        &'a self,
        options: HarnessStreamOptions,
        cx: &'a Context,
    ) -> BoxFuture<'a, Result<(), HarnessError>> {
        Box::pin(async move {
            if self.is_closed() {
                return Err(self.closed_error());
            }
            let mut config = self.config.write().await;
            let previous = config.stream_options.clone();
            config.stream_options = options.clone();
            drop(config);
            self.events
                .emit(
                    HarnessEvent::global(HarnessEventPayload::ConfigUpdate {
                        change: ConfigUpdateChange::StreamOptions {
                            value: options,
                            previous,
                        },
                    }),
                    cx,
                )
                .await;
            Ok(())
        })
    }

    fn get_retry_policy<'a>(
        &'a self,
        _cx: &'a Context,
    ) -> BoxFuture<'a, Result<HarnessRetryPolicy, HarnessError>> {
        Box::pin(async move { Ok(self.config.read().await.retry) })
    }

    fn set_retry_policy<'a>(
        &'a self,
        policy: HarnessRetryPolicy,
        cx: &'a Context,
    ) -> BoxFuture<'a, Result<(), HarnessError>> {
        Box::pin(async move {
            if self.is_closed() {
                return Err(self.closed_error());
            }
            policy
                .validate()
                .map_err(|_| HarnessError::InvalidRetryPolicy {
                    max_retries: policy.max_retries,
                    base_delay_ms: policy.base_delay_ms,
                    message: "retry policy cannot be represented safely".to_owned(),
                })?;
            let mut config = self.config.write().await;
            let previous = config.retry;
            config.retry = policy;
            drop(config);
            self.events
                .emit(
                    HarnessEvent::global(HarnessEventPayload::ConfigUpdate {
                        change: ConfigUpdateChange::RetryPolicy {
                            value: policy,
                            previous,
                        },
                    }),
                    cx,
                )
                .await;
            Ok(())
        })
    }

    fn get_compaction_settings<'a>(
        &'a self,
        _cx: &'a Context,
    ) -> BoxFuture<'a, Result<CompactionSettings, HarnessError>> {
        Box::pin(async move { Ok(self.config.read().await.compaction) })
    }

    fn set_compaction_settings<'a>(
        &'a self,
        settings: CompactionSettings,
        cx: &'a Context,
    ) -> BoxFuture<'a, Result<(), HarnessError>> {
        Box::pin(async move {
            if self.is_closed() {
                return Err(self.closed_error());
            }
            let mut config = self.config.write().await;
            let previous = config.compaction;
            config.compaction = settings;
            drop(config);
            self.events
                .emit(
                    HarnessEvent::global(HarnessEventPayload::ConfigUpdate {
                        change: ConfigUpdateChange::CompactionSettings {
                            value: settings,
                            previous,
                        },
                    }),
                    cx,
                )
                .await;
            Ok(())
        })
    }

    fn get_steering_mode<'a>(
        &'a self,
        _cx: &'a Context,
    ) -> BoxFuture<'a, Result<crate::queue::QueueMode, HarnessError>> {
        Box::pin(async move { Ok(self.config.read().await.steering_mode) })
    }

    fn set_steering_mode<'a>(
        &'a self,
        mode: crate::queue::QueueMode,
        cx: &'a Context,
    ) -> BoxFuture<'a, Result<(), HarnessError>> {
        Box::pin(async move {
            if self.is_closed() {
                return Err(self.closed_error());
            }
            let mut config = self.config.write().await;
            let previous = config.steering_mode;
            config.steering_mode = mode;
            drop(config);
            self.events
                .emit(
                    HarnessEvent::global(HarnessEventPayload::ConfigUpdate {
                        change: ConfigUpdateChange::SteeringMode {
                            value: mode,
                            previous,
                        },
                    }),
                    cx,
                )
                .await;
            Ok(())
        })
    }

    fn get_follow_up_mode<'a>(
        &'a self,
        _cx: &'a Context,
    ) -> BoxFuture<'a, Result<crate::queue::QueueMode, HarnessError>> {
        Box::pin(async move { Ok(self.config.read().await.follow_up_mode) })
    }

    fn set_follow_up_mode<'a>(
        &'a self,
        mode: crate::queue::QueueMode,
        cx: &'a Context,
    ) -> BoxFuture<'a, Result<(), HarnessError>> {
        Box::pin(async move {
            if self.is_closed() {
                return Err(self.closed_error());
            }
            let mut config = self.config.write().await;
            let previous = config.follow_up_mode;
            config.follow_up_mode = mode;
            drop(config);
            self.events
                .emit(
                    HarnessEvent::global(HarnessEventPayload::ConfigUpdate {
                        change: ConfigUpdateChange::FollowUpMode {
                            value: mode,
                            previous,
                        },
                    }),
                    cx,
                )
                .await;
            Ok(())
        })
    }

    fn watch_session<'a>(
        &'a self,
        cx: &'a Context,
    ) -> BoxFuture<'a, Result<WatchHandle<crate::harness::snapshot::SessionSnapshot>, HarnessError>>
    {
        Box::pin(async move {
            let this = Arc::new(self.clone_ref());
            let capture: SnapshotCapture<crate::harness::snapshot::SessionSnapshot> =
                Arc::new(move |context| {
                    let this = Arc::clone(&this);
                    async move { this.session_snapshot(&context).await }.boxed()
                });
            self.events
                .watch_from_snapshot(capture, Arc::new(|_| true), cx)
                .await
        })
    }

    fn hooks(&self) -> &HookRegistry {
        &self.hooks
    }
    fn events(&self) -> &HarnessEventBus {
        &self.events
    }

    fn close<'a>(&'a self, cx: &'a Context) -> BoxFuture<'a, Result<(), HarnessError>> {
        Box::pin(async move {
            // A fault seal stops admissions but deliberately leaves the
            // session open; only explicit close drains it, so the session
            // close below runs even when the runtime already shut down.
            if !self.closed.swap(true, Ordering::AcqRel) {
                let fault = Arc::new(HarnessFault {
                    message: "harness is closed".to_owned(),
                    cause: Box::new(CloseReason),
                });
                self.hooks.close(Arc::clone(&fault));
                self.events.close(Arc::clone(&fault));
                let lanes = self.lane_snapshot().await;
                for lane in lanes {
                    lane.seal(Arc::clone(&fault)).await;
                }
            }
            self.session.close(cx).await.map_err(map_session_error)
        })
    }
}

impl HarnessRuntime {
    fn clone_ref(&self) -> Self {
        Self {
            session: Arc::clone(&self.session),
            models: Arc::clone(&self.models),
            hooks: self.hooks.clone(),
            events: self.events.clone(),
            config: Arc::clone(&self.config),
            lanes: Arc::clone(&self.lanes),
            closed: Arc::clone(&self.closed),
            fault: Arc::clone(&self.fault),
        }
    }

    async fn session_snapshot(
        &self,
        cx: &Context,
    ) -> Result<crate::harness::snapshot::SessionSnapshot, HarnessError> {
        let lanes = self.lanes(cx).await?;
        Ok(crate::harness::snapshot::SessionSnapshot {
            lanes,
            faulted: self.fault.lock().is_ok_and(|slot| slot.is_some()),
        })
    }
}

#[derive(Debug, thiserror::Error)]
#[error("harness close requested")]
struct CloseReason;

fn validate_tools(
    tools: &[Arc<dyn crate::harness::tool::HarnessTool>],
) -> Result<(), HarnessError> {
    let mut names = std::collections::BTreeSet::new();
    for tool in tools {
        let name = tool.name();
        if name.is_empty() || !names.insert(name.to_owned()) {
            return Err(HarnessError::InvalidLane {
                lane: LaneName::new(""),
                reason: "invalid_tools".to_owned(),
                message: "tool names must be non-empty and unique".to_owned(),
            });
        }
    }
    Ok(())
}

fn validate_active_tools(
    active: Option<&[String]>,
    tools: &[Arc<dyn crate::harness::tool::HarnessTool>],
) -> Result<(), HarnessError> {
    let Some(active) = active else {
        return Ok(());
    };
    let available: std::collections::BTreeSet<&str> =
        tools.iter().map(|tool| tool.name()).collect();
    if let Some(name) = active
        .iter()
        .find(|name| !available.contains(name.as_str()))
    {
        return Err(HarnessError::InvalidLane {
            lane: LaneName::new(""),
            reason: "unknown_tool".to_owned(),
            message: format!("active tool {name} is not registered"),
        });
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;
    use std::error::Error;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};

    use futures::future::FutureExt;
    use futures::stream::{self, BoxStream, StreamExt};

    use super::*;
    use crate::harness::api::HarnessModels;
    use crate::harness::event::{EventListener, HarnessEventType};
    use crate::session::{
        HarnessRetryPolicy, HarnessStreamOptions, MemoryStorage, SessionMetadata,
        StorageBackedSession, UuidV7Generator,
    };
    use crate::tool::ToolExecutionMode;

    fn fixture_model() -> pi_ai::Model {
        pi_ai::Model {
            id: "retry-closed-model".to_owned(),
            name: "Retry closed fixture".to_owned(),
            api: "retry-closed-api".to_owned(),
            provider: "retry-closed-provider".to_owned(),
            base_url: String::new(),
            reasoning: false,
            thinking_level_map: None,
            input: Vec::new(),
            cost: pi_ai::ModelCost::default(),
            context_window: 8192,
            max_tokens: 1024,
            headers: None,
            compat: None,
            extra: BTreeMap::default(),
        }
    }

    /// A model store whose provider is never invoked here; streaming returns an
    /// empty stream so an accidental call fails closed instead of panicking.
    struct ClosedModels {
        model: pi_ai::Model,
    }

    impl pi_ai::Provider for ClosedModels {
        fn stream(
            &self,
            _model: &pi_ai::Model,
            _context: pi_ai::Context,
            _options: pi_ai::StreamOptions,
        ) -> BoxStream<'static, Result<pi_ai::AssistantMessageEvent, pi_ai::ProviderError>>
        {
            stream::empty().boxed()
        }
    }

    impl HarnessModels for ClosedModels {
        fn get_model(&self, provider: &str, model_id: &str) -> Option<pi_ai::Model> {
            (self.model.provider == provider && self.model.id == model_id)
                .then(|| self.model.clone())
        }
    }

    async fn build_harness(cx: &Context) -> Result<Arc<dyn AgentHarness>, Box<dyn Error>> {
        let session = StorageBackedSession::new(
            SessionMetadata {
                id: "retry-closed".to_owned(),
                created_at: 1,
                storage_version: MemoryStorage::STORAGE_VERSION,
                cwd: None,
                parent_session_id: None,
                legacy_parent_session_path: None,
            },
            Arc::new(MemoryStorage::new()),
            Arc::new(UuidV7Generator::new()),
            None,
        );
        let models = Arc::new(ClosedModels {
            model: fixture_model(),
        });
        let (harness, _) = AgentHarnessBuilder::create(
            AgentHarnessOptions {
                session,
                models: models.clone(),
                model: models.model.clone(),
                thinking_level: None,
                active_tool_names: None,
                tools: Vec::new(),
                tool_context: None,
                system_prompt: None,
                resources: HarnessResources::default(),
                stream_options: HarnessStreamOptions::default(),
                retry: None,
                compaction: None,
                steering_mode: None,
                follow_up_mode: None,
                tool_execution: ToolExecutionMode::default(),
                to_provider_messages: None,
                entry_projectors: HashMap::new(),
            },
            cx,
        )
        .await?;
        Ok(harness)
    }

    /// A closed harness must reject a retry-policy change with the sealed
    /// error, leave the sealed configuration untouched, and emit no
    /// configuration event — matching the guard every sibling setter uses.
    #[tokio::test]
    async fn closed_harness_rejects_retry_policy_change_and_emits_no_event()
    -> Result<(), Box<dyn Error>> {
        let cx = Context::background();
        let harness = build_harness(&cx).await?;

        let config_updates = Arc::new(AtomicUsize::new(0));
        let observed = Arc::clone(&config_updates);
        let _listener = harness.events().on(HarnessEventType::ConfigUpdate, {
            Arc::new(move |_event: HarnessEvent, _cx: Context| {
                let observed = Arc::clone(&observed);
                async move {
                    observed.fetch_add(1, Ordering::Release);
                }
                .boxed()
            }) as EventListener
        })?;

        // A successful change before close proves the listener is wired, so
        // the later zero is meaningful rather than a dead subscription.
        let pre_close = HarnessRetryPolicy {
            enabled: true,
            max_retries: 5,
            base_delay_ms: 1_500,
        };
        harness.set_retry_policy(pre_close, &cx).await?;
        assert_eq!(
            config_updates.load(Ordering::Acquire),
            1,
            "open harness should emit one configuration event",
        );

        harness.close(&cx).await?;

        let sealed = HarnessRetryPolicy {
            enabled: true,
            max_retries: 7,
            base_delay_ms: 2_500,
        };
        let result = harness.set_retry_policy(sealed, &cx).await;
        assert!(
            matches!(result, Err(HarnessError::Closed { .. })),
            "closed harness must reject a retry-policy change, got {result:?}",
        );
        assert_eq!(
            config_updates.load(Ordering::Acquire),
            1,
            "closed harness must not emit a configuration event",
        );

        let after = harness.get_retry_policy(&cx).await?;
        assert_eq!(
            after, pre_close,
            "closed harness must not mutate the sealed retry policy",
        );
        Ok(())
    }
}
