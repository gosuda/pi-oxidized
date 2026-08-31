//! Terminal lifecycle: capabilities, probes, input, sink, backend, guard, writer.

pub mod backend;
pub mod caps;
pub mod guard;
pub mod input;
pub mod probe;
pub mod sink;
pub mod writer;

pub use backend::{
    ByteAuditReport, GuardedBackend, audit_bytes, encode_full_row_prefix, wrap_synchronized,
};
pub use caps::{
    CellDimensions, ImageProtocol, ImageProtocolOverride, KeyboardProtocol, TerminalCapabilities,
    TerminalCapabilityOverrides, kitty_delete_all, kitty_delete_id,
};
pub use guard::{
    GuardScript, KITTY_KEYBOARD_FLAGS, TerminalGuard, install_panic_emergency_hook,
    write_emergency_restore_bytes,
};
pub use input::{TerminalInput, map_event};
pub use probe::{
    PROBE_FRAGMENT_TIMEOUT, ProbeFeed, ProbeReply, ProbeSession, TerminalTheme,
    background_from_replies, classify_background, detect_terminal_theme, osc_11_query,
    probe_background, probe_background_from_chunks, probe_collect_replies, probe_query_batch,
    probe_write_batch, reinject_bytes_as_events,
};
pub use sink::FrameSink;
pub use writer::{
    COALESCE_WINDOW, Coalescer, ReanchorCause, SettledBlock, SimulatedTxn, TransactionRecorder,
    Tui, Txn, paint_timer_read, paint_timer_reset, set_paint_timer,
};
