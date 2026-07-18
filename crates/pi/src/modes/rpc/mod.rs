//! JSONL RPC framing and wire contracts.

pub mod extension_ui;
pub mod jsonl;
pub mod mode_host;
pub mod server;
pub mod types;

pub use extension_ui::ExtensionUiProxy;
pub use jsonl::{JsonlLineReader, serialize_json_line};
pub use server::{
    BufferSink, ModelCycleResult, OutputGuardSink, RebindCallback, RpcSessionHost, RpcSink,
    ServerOutput, run_rpc_loop,
};
pub use types::{
    BashResult, ForkMessage, RpcCommand, RpcExtensionUiRequest, RpcExtensionUiResponse,
    RpcResponse, RpcResponseData, RpcSessionState, RpcSessionTreeNode, RpcSlashCommand,
    SessionStats, SessionStatsTokens, StreamingBehavior,
};
