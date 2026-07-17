//! Compile-time anchors for public contracts required by the rewrite plan.

pub use pi::VERSION;

pub use pi_ai::{AssistantMessageEvent, Context, Model, Provider, StreamOptions};

pub use pi_agent::{AgentEvent, AgentTool, AgentToolResult, QueueMode};

pub use pi_tui::{Component, EventResult, UiEvent};

pub use pi_ext::protocol::{Frame, FrameKind, PROTOCOL_VERSION};
