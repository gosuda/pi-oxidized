//! Concrete `RpcSessionHost` implementation for `Arc<AgentSessionRuntime>`.
//!
//! Adapts the product session/runtime surface to the trait defined in
//! [`super::server`]. Each async method clones the needed `Arc` so the
//! returned future is `'static` and can be `tokio::spawn`'d.

use std::sync::Arc;

use futures::Future;
use futures::future::BoxFuture;
use pi_agent::{AgentMessage, QueueMode};
use pi_ai::{ImageContent, Model, ModelThinkingLevel};
use std::pin::Pin;

use crate::core::agent_session::AgentSession;
use crate::core::agent_session::events::AgentSessionEvent;
use crate::core::agent_session::extension::ExtensionBindings;
use crate::core::agent_session::model::CycleDirection;
use crate::core::agent_session::prompt::{
    PreflightCallback, PromptOptions, StreamingBehavior as PromptStreamingBehavior,
};
use crate::core::agent_session_runtime::{
    AgentSessionRuntime, ForkOutcome, ForkPosition, NewSessionOptions, RebindSessionCallback,
    SwitchSessionOptions,
};
use crate::core::compaction::CompactionResult;
use crate::core::extension_runtime_set::ExtensionRuntimeSet;
use crate::core::resources::{SlashCommandInfo, SlashCommandSource};
use crate::core::sessions::{SessionEntry, SessionTreeNode};

use super::server::{ModelCycleResult, RebindCallback, RpcSessionHost};
use super::types::{
    BashResult, ForkMessage, RpcSessionState, RpcSessionTreeNode, RpcSlashCommand,
    RpcSlashCommandSource, SessionStats, StreamingBehavior,
};

// ---------------------------------------------------------------------------
// Conversion helpers
// ---------------------------------------------------------------------------

fn convert_tree(nodes: Vec<SessionTreeNode>) -> Vec<RpcSessionTreeNode> {
    nodes
        .into_iter()
        .map(|n| RpcSessionTreeNode {
            entry: n.entry,
            children: convert_tree(n.children),
            label: n.label,
            label_timestamp: n.label_timestamp,
        })
        .collect()
}

fn convert_bash_result(r: crate::core::agent_session::bash::BashResult) -> BashResult {
    BashResult {
        output: r.output,
        exit_code: r.exit_code,
        cancelled: r.cancelled,
        truncated: r.truncated,
        full_output_path: r.full_output_path,
    }
}

fn convert_stats(s: crate::core::agent_session::stats::SessionStats) -> SessionStats {
    use crate::modes::rpc::types::{ContextUsage, SessionStatsTokens};
    SessionStats {
        session_file: s.session_file,
        session_id: s.session_id,
        user_messages: s.user_messages,
        assistant_messages: s.assistant_messages,
        tool_calls: s.tool_calls,
        tool_results: s.tool_results,
        total_messages: s.total_messages,
        tokens: SessionStatsTokens {
            input: s.tokens.input,
            output: s.tokens.output,
            cache_read: s.tokens.cache_read,
            cache_write: s.tokens.cache_write,
            total: s.tokens.total,
        },
        cost: s.cost,
        context_usage: s.context_usage.map(|c| ContextUsage {
            tokens: c.tokens,
            context_window: c.context_window,
            percent: c.percent,
        }),
    }
}
fn convert_slash_command(command: SlashCommandInfo) -> RpcSlashCommand {
    RpcSlashCommand {
        name: command.name,
        description: command.description,
        source: match command.source {
            SlashCommandSource::Extension => RpcSlashCommandSource::Extension,
            SlashCommandSource::Prompt => RpcSlashCommandSource::Prompt,
            SlashCommandSource::Skill => RpcSlashCommandSource::Skill,
        },
        source_info: command.source_info.into(),
    }
}

// ---------------------------------------------------------------------------
// RpcSessionHost for Arc<AgentSessionRuntime>
// ---------------------------------------------------------------------------

impl RpcSessionHost for Arc<AgentSessionRuntime> {
    fn prompt(
        &self,
        message: String,
        images: Vec<ImageContent>,
        streaming_behavior: Option<StreamingBehavior>,
        preflight: PreflightCallback,
    ) -> BoxFuture<'static, Result<(), String>> {
        let session = self.session();
        Box::pin(async move {
            let opts = PromptOptions {
                images,
                streaming_behavior: streaming_behavior.map(|sb| match sb {
                    StreamingBehavior::Steer => PromptStreamingBehavior::Steer,
                    StreamingBehavior::FollowUp => PromptStreamingBehavior::FollowUp,
                }),
                source: Some("rpc".into()),
                preflight_result: Some(preflight),
                ..Default::default()
            };
            session
                .prompt(&message, opts)
                .await
                .map_err(|e| e.to_string())
        })
    }

    fn steer(
        &self,
        message: String,
        images: Vec<ImageContent>,
    ) -> BoxFuture<'static, Result<(), String>> {
        let session = self.session();
        Box::pin(async move { session.steer(&message, images).map_err(|e| e.to_string()) })
    }

    fn follow_up(
        &self,
        message: String,
        images: Vec<ImageContent>,
    ) -> BoxFuture<'static, Result<(), String>> {
        let session = self.session();
        Box::pin(async move {
            session
                .follow_up(&message, images)
                .map_err(|e| e.to_string())
        })
    }

    fn abort(&self) -> BoxFuture<'static, ()> {
        let session = self.session();
        Box::pin(async move {
            session.abort().await;
        })
    }

    fn get_state(&self) -> BoxFuture<'static, RpcSessionState> {
        let session = self.session();
        Box::pin(async move {
            let m = session.model();
            RpcSessionState {
                model: if m.id.is_empty() { None } else { Some(m) },
                thinking_level: session.thinking_level(),
                is_streaming: session.is_streaming(),
                is_compacting: session.is_compacting(),
                steering_mode: session.steering_mode(),
                follow_up_mode: session.follow_up_mode(),
                session_file: session.session_file().await,
                session_id: session.session_id().await,
                session_name: session.session_name().await,
                auto_compaction_enabled: session.auto_compaction_enabled(),
                message_count: session.message_count() as u64,
                pending_message_count: session.pending_message_count() as u64,
            }
        })
    }

    fn get_available_models(&self) -> BoxFuture<'static, Vec<Model>> {
        let session = self.session();
        Box::pin(async move {
            if let Some(mr) = session.model_runtime_handle() {
                return mr.get_available(None).await.unwrap_or_default();
            }
            Vec::new()
        })
    }

    fn set_model(&self, model: Model) -> BoxFuture<'static, Result<(), String>> {
        let session = self.session();
        Box::pin(async move { session.set_model(model).await.map_err(|e| e.to_string()) })
    }

    fn cycle_model(&self) -> BoxFuture<'static, Option<ModelCycleResult>> {
        let session = self.session();
        Box::pin(async move {
            session
                .cycle_model(CycleDirection::Forward)
                .await
                .map(|r| ModelCycleResult {
                    model: r.model,
                    thinking_level: r.thinking_level,
                    is_scoped: r.is_scoped,
                })
        })
    }

    fn set_thinking_level(&self, level: ModelThinkingLevel) -> BoxFuture<'static, bool> {
        let session = self.session();
        Box::pin(async move { session.set_thinking_level(level).await })
    }

    fn cycle_thinking_level(&self) -> BoxFuture<'static, Option<ModelThinkingLevel>> {
        let session = self.session();
        Box::pin(async move { session.cycle_thinking_level().await })
    }

    fn set_steering_mode(&self, mode: QueueMode) -> BoxFuture<'static, ()> {
        let session = self.session();
        Box::pin(async move {
            session.set_steering_mode(mode);
        })
    }

    fn set_follow_up_mode(&self, mode: QueueMode) -> BoxFuture<'static, ()> {
        let session = self.session();
        Box::pin(async move {
            session.set_follow_up_mode(mode);
        })
    }

    fn compact(
        &self,
        custom_instructions: Option<String>,
    ) -> BoxFuture<'static, Result<CompactionResult, String>> {
        let session = self.session();
        Box::pin(async move {
            session
                .compact(custom_instructions.as_deref())
                .await
                .map_err(|e| e.to_string())
        })
    }

    fn set_auto_compaction(&self, enabled: bool) -> BoxFuture<'static, ()> {
        let session = self.session();
        Box::pin(async move {
            session.set_auto_compaction_enabled(enabled);
        })
    }

    fn set_auto_retry(&self, enabled: bool) -> BoxFuture<'static, ()> {
        let session = self.session();
        Box::pin(async move {
            session.set_auto_retry_enabled(enabled);
        })
    }

    fn abort_retry(&self) -> BoxFuture<'static, ()> {
        let session = self.session();
        Box::pin(async move {
            session.abort_retry();
        })
    }

    fn execute_bash(
        &self,
        command: String,
        exclude_from_context: Option<bool>,
    ) -> BoxFuture<'static, Result<BashResult, String>> {
        let session = self.session();
        Box::pin(async move {
            let opts = crate::core::agent_session::bash::ExecuteBashOptions {
                exclude_from_context: exclude_from_context.unwrap_or(false),
                ..Default::default()
            };
            session
                .execute_bash(&command, None::<fn(&str)>, opts)
                .await
                .map(convert_bash_result)
                .map_err(|e| e.to_string())
        })
    }

    fn abort_bash(&self) -> BoxFuture<'static, ()> {
        let session = self.session();
        Box::pin(async move {
            session.abort_bash();
        })
    }

    fn get_session_stats(&self) -> BoxFuture<'static, SessionStats> {
        let session = self.session();
        Box::pin(async move { convert_stats(session.get_session_stats().await) })
    }

    fn export_to_html(
        &self,
        output_path: Option<String>,
    ) -> BoxFuture<'static, Result<String, String>> {
        let session = self.session();
        Box::pin(async move {
            session
                .export_to_html(output_path.as_deref(), None)
                .await
                .map_err(|e| e.to_string())
        })
    }

    fn set_session_name(&self, name: String) -> BoxFuture<'static, Result<(), String>> {
        let session = self.session();
        Box::pin(async move {
            session
                .set_session_name(&name)
                .await
                .map_err(|e| e.to_string())
        })
    }

    fn new_session(
        &self,
        parent_session: Option<String>,
    ) -> BoxFuture<'static, Result<bool, String>> {
        let this = Arc::clone(self);
        Box::pin(async move {
            this.as_ref()
                .new_session(NewSessionOptions { parent_session })
                .await
                .map(|o| o.cancelled)
                .map_err(|e| e.to_string())
        })
    }

    fn switch_session(&self, session_path: String) -> BoxFuture<'static, Result<bool, String>> {
        let this = Arc::clone(self);
        Box::pin(async move {
            this.as_ref()
                .switch_session(&session_path, SwitchSessionOptions::default())
                .await
                .map(|o| o.cancelled)
                .map_err(|e| e.to_string())
        })
    }

    fn fork(
        &self,
        entry_id: String,
        position: ForkPosition,
    ) -> BoxFuture<'static, Result<ForkOutcome, String>> {
        let this = Arc::clone(self);
        Box::pin(async move {
            this.as_ref()
                .fork(&entry_id, position)
                .await
                .map_err(|e| e.to_string())
        })
    }

    fn get_entries(&self) -> BoxFuture<'static, Vec<SessionEntry>> {
        let sm = self.session().session_manager();
        Box::pin(async move { sm.lock().await.get_entries().into_iter().cloned().collect() })
    }

    fn get_leaf_id(&self) -> BoxFuture<'static, Option<String>> {
        let sm = self.session().session_manager();
        Box::pin(async move { sm.lock().await.get_leaf_id().map(str::to_owned) })
    }

    fn get_tree(&self) -> BoxFuture<'static, Vec<RpcSessionTreeNode>> {
        let sm = self.session().session_manager();
        Box::pin(async move { convert_tree(sm.lock().await.get_tree()) })
    }

    fn get_fork_messages(&self) -> BoxFuture<'static, Vec<ForkMessage>> {
        let session = self.session();
        Box::pin(async move {
            session
                .get_user_messages_for_forking()
                .await
                .into_iter()
                .map(|m| ForkMessage {
                    entry_id: m.entry_id,
                    text: m.text,
                })
                .collect()
        })
    }

    fn get_last_assistant_text(&self) -> BoxFuture<'static, Option<String>> {
        let session = self.session();
        Box::pin(async move { session.get_last_assistant_text() })
    }

    fn get_messages(&self) -> BoxFuture<'static, Vec<AgentMessage>> {
        let session = self.session();
        Box::pin(async move { session.messages() })
    }

    fn get_commands(&self) -> BoxFuture<'static, Vec<RpcSlashCommand>> {
        let session = self.session();
        Box::pin(async move {
            session
                .slash_commands()
                .into_iter()
                .map(convert_slash_command)
                .collect()
        })
    }

    fn host_extension_runner(&self) -> Option<Arc<ExtensionRuntimeSet>> {
        self.session().host_extension_runner()
    }

    fn subscribe(
        &self,
        listener: Arc<dyn Fn(&AgentSessionEvent) + Send + Sync>,
    ) -> Box<dyn Fn() + Send + Sync> {
        let session = self.session();
        let unsub = session.subscribe(move |event: &AgentSessionEvent| {
            listener(event);
        });
        Box::new(unsub)
    }

    fn register_backpressure_hook(
        &self,
        hook: Arc<dyn Fn() -> BoxFuture<'static, ()> + Send + Sync>,
    ) -> Box<dyn Fn() + Send + Sync> {
        self.session().register_event_backpressure_hook(hook)
    }

    fn bind_extensions_rpc(
        &self,
        bindings: ExtensionBindings,
    ) -> BoxFuture<'static, Result<(), String>> {
        let session = self.session();
        Box::pin(async move {
            session
                .bind_extensions(bindings)
                .await
                .map_err(|e| e.to_string())
        })
    }

    fn dispose(&self) -> BoxFuture<'static, ()> {
        let this = Arc::clone(self);
        Box::pin(async move {
            this.as_ref().dispose().await;
        })
    }

    fn set_rebind(&self, callback: Option<RebindCallback>) {
        let adapted = callback.map(|cb| {
            Arc::new(move |_session: Arc<AgentSession>| {
                let cb = Arc::clone(&cb);
                let fut: Pin<Box<dyn Future<Output = ()> + Send + 'static>> =
                    Box::pin(async move { cb().await });
                fut
            }) as RebindSessionCallback
        });
        self.as_ref().set_rebind_session(adapted);
    }
}
