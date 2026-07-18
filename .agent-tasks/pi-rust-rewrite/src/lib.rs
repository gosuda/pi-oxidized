//! Compile-time anchors for public contracts required by the rewrite plan.

pub use pi::VERSION;

pub use pi_ai::{
    AssistantMessageEvent, Context, Message, Model, Provider, ProviderError, StreamOptions,
};

pub use pi_agent::{AgentEvent, AgentTool, AgentToolResult, QueueMode};

pub use pi_tui::component::{Component, EventResult, UiEvent};

pub use pi_ext::protocol::{Frame, FrameKind, PROTOCOL_VERSION};
