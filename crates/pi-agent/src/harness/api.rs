//! Public harness and lane boundary.
//!
//! This module declares the reusable API contract only. It does not construct
//! or drive a runtime; the runtime owner supplies the implementation of
//! [`AgentHarness`] and the `AgentHarnessBuilder` integration point.

use std::collections::HashMap;
use std::sync::Arc;

use futures::future::BoxFuture;

use crate::context::Context;
use crate::message::AgentMessage;
use crate::queue::QueueMode;
use crate::session::{
    BranchScan, CompactionSettings, Entry, EntryId, EntryProjector, HarnessRetryPolicy, LaneName,
    ModelIdentity, OperationId, OperationResultRecord, Session,
};
use crate::tool::ToolExecutionMode;

use super::bus::{HarnessEventBus, WatchHandle};
use super::hooks::HookRegistry;
use super::result::{
    AbortRequestResult, AbortResult, CancelQueuedResult, CompactionResult, DriveResult,
    HarnessError, LaneExecutionInfo, LaneInfo, NavigationResult, OperationAdmissionResult,
    QueueResult, RecordUsageResult, ResumeResult, RunResult,
};
use super::snapshot::{LaneSnapshot, SessionSnapshot};
use super::stream::ToProviderMessages;
use super::tool::{HarnessTool, ToolContextSource, ToolContextValue};

/// Model lookup plus provider streaming used by the harness boundary.
///
/// Product code implements this trait on its composed model runtime. The
/// lookup is exact: a missing provider/model pair returns `None`, and the
/// inherited [`pi_ai::Provider::stream`] operation owns provider selection and
/// authentication.
pub trait HarnessModels: pi_ai::Provider {
    /// Resolves one provider/model identity from the current model snapshot.
    fn get_model(&self, provider: &str, model_id: &str) -> Option<pi_ai::Model>;
}

/// One lane of durable transcript and operation state.
pub trait AgentLane: Send + Sync {
    /// Returns this lane's stable name.
    fn name(&self) -> &LaneName;

    /// Reads the current durable tip.
    fn get_tip_id<'a>(
        &'a self,
        cx: &'a Context,
    ) -> BoxFuture<'a, Result<Option<EntryId>, HarnessError>>;
    /// Scans entries in this lane's branch.
    fn find_entries<'a>(
        &'a self,
        query: Option<&'a BranchScan>,
        cx: &'a Context,
    ) -> BoxFuture<'a, Result<Vec<Entry>, HarnessError>>;
    /// Finds the first entry matching a branch query.
    fn find_entry<'a>(
        &'a self,
        query: Option<&'a BranchScan>,
        cx: &'a Context,
    ) -> BoxFuture<'a, Result<Option<Entry>, HarnessError>>;
    /// Reads a settled operation result by id.
    fn get_result<'a>(
        &'a self,
        operation_id: &'a OperationId,
        cx: &'a Context,
    ) -> BoxFuture<'a, Result<Option<OperationResultRecord>, HarnessError>>;

    /// Appends a durable message outside an active operation.
    fn append_message<'a>(
        &'a self,
        message: AgentMessage,
        cx: &'a Context,
    ) -> BoxFuture<'a, Result<EntryId, HarnessError>>;
    /// Appends a durable custom entry outside an active operation.
    fn append_custom_entry<'a>(
        &'a self,
        custom_type: &'a str,
        data: Option<serde_json::Value>,
        cx: &'a Context,
    ) -> BoxFuture<'a, Result<EntryId, HarnessError>>;

    /// Admits an operation without driving it.
    fn accept<'a>(
        &'a self,
        request: OperationRequest,
        cx: &'a Context,
    ) -> BoxFuture<'a, OperationAdmissionResult>;
    /// Drives one admitted operation step.
    fn drive<'a>(&'a self, options: DriveOptions, cx: &'a Context) -> BoxFuture<'a, DriveResult>;
    /// Requests cancellation for one operation.
    fn request_abort<'a>(
        &'a self,
        operation_id: &'a OperationId,
        cx: &'a Context,
    ) -> BoxFuture<'a, AbortRequestResult>;
    /// Inspects the current execution state.
    fn inspect_execution<'a>(
        &'a self,
        cx: &'a Context,
    ) -> BoxFuture<'a, Result<LaneExecutionInfo, HarnessError>>;

    /// Accepts and drives a prompt operation to settlement or suspension.
    fn prompt<'a>(&'a self, prompt: PromptInput, cx: &'a Context) -> BoxFuture<'a, RunResult>;
    /// Accepts and drives a configured skill operation.
    fn skill<'a>(
        &'a self,
        name: &'a str,
        additional_instructions: Option<&'a str>,
        cx: &'a Context,
    ) -> BoxFuture<'a, RunResult>;
    /// Accepts and drives a configured prompt-template operation.
    fn prompt_from_template<'a>(
        &'a self,
        name: &'a str,
        args: &'a [String],
        cx: &'a Context,
    ) -> BoxFuture<'a, RunResult>;
    /// Accepts and drives a compaction operation.
    fn compact<'a>(
        &'a self,
        custom_instructions: Option<&'a str>,
        cx: &'a Context,
    ) -> BoxFuture<'a, CompactionResult>;
    /// Accepts and drives a navigation operation.
    fn navigate_tree<'a>(
        &'a self,
        target: Option<&'a EntryId>,
        options: NavigateOptions,
        cx: &'a Context,
    ) -> BoxFuture<'a, NavigationResult>;
    /// Resumes the most recent suspended operation.
    fn resume<'a>(&'a self, cx: &'a Context) -> BoxFuture<'a, ResumeResult>;
    /// Aborts the current operation and settles its abort boundary.
    fn abort<'a>(&'a self, cx: &'a Context) -> BoxFuture<'a, AbortResult>;

    /// Queues steering input for the current run.
    fn steer<'a>(&'a self, message: QueueInput, cx: &'a Context) -> BoxFuture<'a, QueueResult>;
    /// Queues follow-up input for the current run.
    fn follow_up<'a>(&'a self, message: QueueInput, cx: &'a Context) -> BoxFuture<'a, QueueResult>;
    /// Queues input for the next run.
    fn next_run<'a>(&'a self, message: QueueInput, cx: &'a Context) -> BoxFuture<'a, QueueResult>;
    /// Cancels one queued entry by its reserved id.
    fn cancel_queued<'a>(
        &'a self,
        entry: &'a EntryId,
        cx: &'a Context,
    ) -> BoxFuture<'a, CancelQueuedResult>;

    /// Records provider usage for this lane.
    fn record_usage<'a>(
        &'a self,
        usage: pi_ai::Usage,
        options: RecordUsageOptions,
        cx: &'a Context,
    ) -> BoxFuture<'a, RecordUsageResult>;

    /// Waits until this lane has no active operation.
    fn wait_for_idle<'a>(&'a self, cx: &'a Context) -> BoxFuture<'a, Result<(), HarnessError>>;
    /// Runs a job after this lane becomes idle.
    fn run_when_idle<'a>(
        &'a self,
        job: IdleJob,
        cx: &'a Context,
    ) -> BoxFuture<'a, Result<(), HarnessError>>;

    /// Resolves the configured model for this lane.
    fn get_model<'a>(
        &'a self,
        cx: &'a Context,
    ) -> BoxFuture<'a, Result<Option<pi_ai::Model>, HarnessError>>;
    /// Changes the model used by subsequent operations.
    fn set_model<'a>(
        &'a self,
        model: ModelIdentity,
        cx: &'a Context,
    ) -> BoxFuture<'a, Result<(), HarnessError>>;
    /// Reads the configured thinking level.
    fn get_thinking_level<'a>(
        &'a self,
        cx: &'a Context,
    ) -> BoxFuture<'a, Result<pi_ai::ModelThinkingLevel, HarnessError>>;
    /// Changes the thinking level used by subsequent operations.
    fn set_thinking_level<'a>(
        &'a self,
        level: pi_ai::ModelThinkingLevel,
        cx: &'a Context,
    ) -> BoxFuture<'a, Result<(), HarnessError>>;
    /// Reads the active tool names.
    fn get_active_tools<'a>(
        &'a self,
        cx: &'a Context,
    ) -> BoxFuture<'a, Result<Vec<String>, HarnessError>>;
    /// Changes the active tool names.
    fn set_active_tools<'a>(
        &'a self,
        names: Vec<String>,
        cx: &'a Context,
    ) -> BoxFuture<'a, Result<(), HarnessError>>;

    /// Creates a watcher for this lane's snapshot and events.
    fn watch<'a>(
        &'a self,
        cx: &'a Context,
    ) -> BoxFuture<'a, Result<WatchHandle<LaneSnapshot>, HarnessError>>;
}

/// Harness attached to one durable session.
pub trait AgentHarness: Send + Sync {
    /// Acquires or creates a lane.
    fn lane<'a>(
        &'a self,
        name: &'a LaneName,
        options: AcquireLaneOptions,
        cx: &'a Context,
    ) -> BoxFuture<'a, Result<Arc<dyn AgentLane>, HarnessError>>;
    /// Lists lanes in the session.
    fn lanes<'a>(&'a self, cx: &'a Context) -> BoxFuture<'a, Result<Vec<LaneInfo>, HarnessError>>;

    /// Reads the session name.
    fn get_name<'a>(
        &'a self,
        cx: &'a Context,
    ) -> BoxFuture<'a, Result<Option<String>, HarnessError>>;
    /// Changes the session name.
    fn set_name<'a>(
        &'a self,
        name: Option<&'a str>,
        cx: &'a Context,
    ) -> BoxFuture<'a, Result<(), HarnessError>>;
    /// Reads a label for a tree entry.
    fn get_label<'a>(
        &'a self,
        target: &'a EntryId,
        cx: &'a Context,
    ) -> BoxFuture<'a, Result<Option<String>, HarnessError>>;
    /// Changes or clears a tree-entry label.
    fn set_label<'a>(
        &'a self,
        target: &'a EntryId,
        label: Option<&'a str>,
        cx: &'a Context,
    ) -> BoxFuture<'a, Result<(), HarnessError>>;

    /// Reads the configured harness tools.
    fn get_tools<'a>(
        &'a self,
        cx: &'a Context,
    ) -> BoxFuture<'a, Result<Vec<Arc<dyn HarnessTool>>, HarnessError>>;
    /// Replaces the configured harness tools.
    fn set_tools<'a>(
        &'a self,
        tools: Vec<Arc<dyn HarnessTool>>,
        cx: &'a Context,
    ) -> BoxFuture<'a, Result<(), HarnessError>>;
    /// Reads configured skills and prompt templates.
    fn get_resources<'a>(
        &'a self,
        cx: &'a Context,
    ) -> BoxFuture<'a, Result<HarnessResources, HarnessError>>;
    /// Replaces configured skills and prompt templates.
    fn set_resources<'a>(
        &'a self,
        resources: HarnessResources,
        cx: &'a Context,
    ) -> BoxFuture<'a, Result<(), HarnessError>>;
    /// Reads provider stream options for future operations.
    fn get_stream_options<'a>(
        &'a self,
        cx: &'a Context,
    ) -> BoxFuture<'a, Result<crate::session::HarnessStreamOptions, HarnessError>>;
    /// Replaces provider stream options for future operations.
    fn set_stream_options<'a>(
        &'a self,
        options: crate::session::HarnessStreamOptions,
        cx: &'a Context,
    ) -> BoxFuture<'a, Result<(), HarnessError>>;
    /// Reads whole-request retry policy for future operations.
    fn get_retry_policy<'a>(
        &'a self,
        cx: &'a Context,
    ) -> BoxFuture<'a, Result<HarnessRetryPolicy, HarnessError>>;
    /// Validates and replaces whole-request retry policy for future operations.
    fn set_retry_policy<'a>(
        &'a self,
        policy: HarnessRetryPolicy,
        cx: &'a Context,
    ) -> BoxFuture<'a, Result<(), HarnessError>>;
    /// Reads compaction settings for future operations.
    fn get_compaction_settings<'a>(
        &'a self,
        cx: &'a Context,
    ) -> BoxFuture<'a, Result<CompactionSettings, HarnessError>>;
    /// Replaces compaction settings for future operations.
    fn set_compaction_settings<'a>(
        &'a self,
        settings: CompactionSettings,
        cx: &'a Context,
    ) -> BoxFuture<'a, Result<(), HarnessError>>;
    /// Reads the steering queue mode.
    fn get_steering_mode<'a>(
        &'a self,
        cx: &'a Context,
    ) -> BoxFuture<'a, Result<QueueMode, HarnessError>>;
    /// Replaces the steering queue mode.
    fn set_steering_mode<'a>(
        &'a self,
        mode: QueueMode,
        cx: &'a Context,
    ) -> BoxFuture<'a, Result<(), HarnessError>>;
    /// Reads the follow-up queue mode.
    fn get_follow_up_mode<'a>(
        &'a self,
        cx: &'a Context,
    ) -> BoxFuture<'a, Result<QueueMode, HarnessError>>;
    /// Replaces the follow-up queue mode.
    fn set_follow_up_mode<'a>(
        &'a self,
        mode: QueueMode,
        cx: &'a Context,
    ) -> BoxFuture<'a, Result<(), HarnessError>>;

    /// Creates a watcher for session-level events and snapshots.
    fn watch_session<'a>(
        &'a self,
        cx: &'a Context,
    ) -> BoxFuture<'a, Result<WatchHandle<SessionSnapshot>, HarnessError>>;
    /// Returns the ordered hook registry.
    fn hooks(&self) -> &HookRegistry;
    /// Returns the ordered event bus.
    fn events(&self) -> &HarnessEventBus;
    /// Closes the harness and rejects subsequent work.
    fn close<'a>(&'a self, cx: &'a Context) -> BoxFuture<'a, Result<(), HarnessError>>;
}

/// Marker for the runtime's constructor integration.
///
/// The runtime owner implements the constructor at
/// `crates/pi-agent/src/harness/runtime/harness.rs` (candidate
/// `createAgentHarness`, `agent-harness.ts:54,622`) as an inherent async
/// method with signature
/// `pub async fn create(options: AgentHarnessOptions, cx: &Context)
/// -> Result<(Arc<dyn AgentHarness>, Vec<OpenOperation>), HarnessError>`.
/// No body is declared here because construction requires the real drive,
/// storage-restore, event, and hook implementations.
pub struct AgentHarnessBuilder;

/// Options used to attach a harness to one durable session.
pub struct AgentHarnessOptions {
    /// Session repository object to attach.
    pub session: Arc<dyn Session>,
    /// Product model runtime implementing lookup and provider streaming.
    pub models: Arc<dyn HarnessModels>,
    /// Initial model captured for the default lane.
    pub model: pi_ai::Model,
    /// Initial thinking level for the default lane. The runtime resolves the
    /// default when this is omitted.
    pub thinking_level: Option<pi_ai::ModelThinkingLevel>,
    /// Initial active tool-name filter.
    pub active_tool_names: Option<Vec<String>>,
    /// Harness tools available to operations.
    pub tools: Vec<Arc<dyn HarnessTool>>,
    /// Optional process-local tool context source.
    pub tool_context: Option<ToolContextSource>,
    /// Optional process-local system-prompt source.
    pub system_prompt: Option<SystemPromptSource>,
    /// Skills and prompt templates available to the harness.
    pub resources: HarnessResources,
    /// Provider stream options captured for future operations.
    pub stream_options: crate::session::HarnessStreamOptions,
    /// Optional whole-request retry policy. The runtime validates it before
    /// installing it and uses the default when it is absent.
    pub retry: Option<HarnessRetryPolicy>,
    /// Optional compaction settings override.
    pub compaction: Option<CompactionSettings>,
    /// Optional steering queue mode override.
    pub steering_mode: Option<QueueMode>,
    /// Optional follow-up queue mode override.
    pub follow_up_mode: Option<QueueMode>,
    /// Tool scheduling mode for operations.
    pub tool_execution: ToolExecutionMode,
    /// Optional conversion from transcript messages to provider messages.
    pub to_provider_messages: Option<ToProviderMessages>,
    /// Per-custom-entry projectors used during context reconstruction.
    pub entry_projectors: HashMap<String, EntryProjector>,
}

/// Options for acquiring a lane.
#[derive(Clone, Debug, Default)]
pub struct AcquireLaneOptions {
    /// Optional branch creation point. `Some(None)` means the branch root.
    pub create_at: Option<Option<EntryId>>,
}

/// Options for one low-level drive step.
#[derive(Clone, Debug)]
pub struct DriveOptions {
    /// Operation to drive.
    pub operation_id: OperationId,
    /// Whether the drive may wait for a retry timestamp.
    pub wait_for_retry: bool,
    /// Whether the drive may poll a deferred provider response.
    pub poll_deferred: bool,
}

/// Options for tree navigation.
#[derive(Clone, Debug, Default)]
pub struct NavigateOptions {
    /// Whether navigation should summarize the detached span.
    pub summarize: Option<bool>,
    /// Optional label for the resulting tree location.
    pub label: Option<String>,
    /// Optional custom summary instructions.
    pub custom_instructions: Option<String>,
}

/// Options for recording provider usage.
#[derive(Clone, Debug, Default)]
pub struct RecordUsageOptions {
    /// Entry associated with the usage, if known.
    pub entry_id: Option<EntryId>,
    /// Arbitrary usage details.
    pub details: Option<serde_json::Value>,
}

/// Prompt input accepted by the single Rust prompt operation.
#[derive(Clone, Debug)]
pub enum PromptInput {
    /// User text with optional image blocks.
    Text {
        /// Prompt text.
        text: String,
        /// Image attachments.
        images: Vec<pi_ai::ImageContent>,
    },
    /// One or more already-formed transcript messages.
    Messages(Vec<AgentMessage>),
}

/// Queue input accepted by steering, follow-up, and next-run queues.
#[derive(Clone, Debug)]
pub enum QueueInput {
    /// User text with optional image blocks.
    Text {
        /// Message text.
        text: String,
        /// Image attachments.
        images: Vec<pi_ai::ImageContent>,
    },
    /// One already-formed transcript message.
    Message(AgentMessage),
}

/// Operation request accepted by low-level admission.
#[derive(Clone, Debug)]
pub enum OperationRequest {
    /// Run a prompt operation.
    Prompt {
        /// Optional caller-provided operation id.
        operation_id: Option<OperationId>,
        /// Prompt payload.
        prompt: PromptInput,
    },
    /// Run a configured skill operation.
    Skill {
        /// Optional caller-provided operation id.
        operation_id: Option<OperationId>,
        /// Skill name.
        name: String,
        /// Additional skill instructions.
        additional_instructions: Option<String>,
    },
    /// Run a configured prompt-template operation.
    PromptTemplate {
        /// Optional caller-provided operation id.
        operation_id: Option<OperationId>,
        /// Template name.
        name: String,
        /// Template arguments.
        args: Vec<String>,
    },
    /// Run a compaction operation.
    Compaction {
        /// Optional caller-provided operation id.
        operation_id: Option<OperationId>,
        /// Additional compaction instructions.
        custom_instructions: Option<String>,
    },
    /// Run a navigation operation.
    Navigation {
        /// Optional caller-provided operation id.
        operation_id: Option<OperationId>,
        /// Target entry, or `None` for the branch root.
        target_id: Option<EntryId>,
        /// Navigation options.
        options: NavigateOptions,
    },
}

/// Job deferred until a lane becomes idle.
pub type IdleJob = Box<dyn FnOnce(Context) -> BoxFuture<'static, ()> + Send>;

/// Process-local system-prompt source.
pub type SystemPromptSource = Arc<
    dyn Fn(Option<ToolContextValue>, Context) -> BoxFuture<'static, Result<String, HarnessError>>
        + Send
        + Sync,
>;

/// Skills and prompt templates configured for the harness.
#[derive(Clone, Debug, Default)]
pub struct HarnessResources {
    /// Prompt templates available by name.
    pub prompt_templates: Vec<PromptTemplate>,
    /// Skills available by name.
    pub skills: Vec<Skill>,
}

/// One named skill resource.
#[derive(Clone, Debug)]
pub struct Skill {
    /// Stable resource name.
    pub name: String,
    /// Human-readable description.
    pub description: String,
    /// Skill instructions.
    pub content: String,
    /// Source path for diagnostics.
    pub file_path: String,
    /// Whether the skill may invoke a model.
    pub disable_model_invocation: bool,
}

/// One named prompt template resource.
#[derive(Clone, Debug)]
pub struct PromptTemplate {
    /// Stable resource name.
    pub name: String,
    /// Optional human-readable description.
    pub description: Option<String>,
    /// Template content.
    pub content: String,
}
