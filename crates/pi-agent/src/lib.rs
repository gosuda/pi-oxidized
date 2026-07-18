//! Agent turn loop, queues, tool scheduling, and events.
//!
//! Phase 2 S0 exposes the public wire contracts shared by the agent loop and
//! product layers. Runtime loop modules are layered on top of this surface.

pub mod agent;
pub mod bus;
pub mod config;
pub mod drain;
pub mod error;
pub mod event;
pub mod message;
pub mod queue;
pub mod run;
pub mod schedule;
pub mod state;
pub mod tool;

pub use agent::{Agent, AgentOptions};
pub use bus::{
    AGENT_EVENT_CAPACITY, AgentEventSink, AgentEventSubscription, EXTENSION_EVENT_CAPACITY,
    EventSink, ExtensionEvent, ExtensionSubscription,
};
pub use config::{
    AfterToolCall, AfterToolCallContext, AfterToolCallResult, AgentContext, AgentLoopConfig,
    AgentLoopTurnUpdate, BeforeToolCall, BeforeToolCallContext, BeforeToolCallResult, ConvertToLlm,
    GetApiKey, GetMessages, PrepareNextTurn, PrepareNextTurnContext, ShouldStopAfterTurn,
    ShouldStopAfterTurnContext, TransformContext, build_stream_options,
    default_convert_to_llm_hook,
};
pub use drain::{DRAIN_EVENT_CAPACITY, DrainItem, ProviderDrain};
pub use error::{AgentLoopError, ToolError};
pub use event::AgentEvent;
pub use message::{
    AgentMessage, CustomAgentMessage, default_convert_to_llm, now_millis, user_text,
};
pub use queue::{PendingMessageQueue, QueueMode};
pub use run::{RunIo, run_agent_loop, run_agent_loop_continue};
pub use schedule::{
    EmitAgentEvent, ExecutedToolCallBatch, MAX_PARALLEL_TOOL_CALLS, PARALLEL_TOOL_UPDATE_CAPACITY,
    execute_tool_calls, fail_tool_calls_from_truncated_message, should_terminate_tool_batch,
};
pub use state::{AgentState, AgentStateSnapshot};
pub use tool::{
    AgentTool, AgentToolResult, ToolExecutionMode, ToolUpdates, error_tool_result, to_pi_tool,
};

/// Re-export of the provider crate used by agent contracts.
pub use pi_ai;
