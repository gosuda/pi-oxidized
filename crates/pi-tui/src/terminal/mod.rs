//! Terminal lifecycle: capabilities, probes, input, sink, backend, guard, writer.
use std::fmt;
use std::str::FromStr;

use serde::{Deserialize, Serialize};

/// Presentation geometry owned by the terminal lifecycle.
///
/// Regular mode preserves the inline transcript/editor surface. Fullscreen
/// mode owns the complete terminal viewport while the product retains the
/// document and overlay state.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ScreenMode {
    /// Keep the terminal's inline viewport and scrollback.
    #[default]
    Regular,
    /// Own the complete terminal viewport.
    Fullscreen,
}

impl ScreenMode {
    /// Return the stable serialized spelling.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Regular => "regular",
            Self::Fullscreen => "fullscreen",
        }
    }
}

impl fmt::Display for ScreenMode {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.as_str())
    }
}

impl FromStr for ScreenMode {
    type Err = String;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        match value {
            "regular" => Ok(Self::Regular),
            "fullscreen" => Ok(Self::Fullscreen),
            other => Err(format!(
                "invalid screen mode {other:?}; expected regular or fullscreen"
            )),
        }
    }
}

pub mod backend;
pub mod caps;
pub mod guard;
pub mod input;
pub mod probe;
pub mod session;
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
    PROBE_FRAGMENT_TIMEOUT, ProbeCollector, QueryKind, TerminalTheme, background_from_replies,
    classify_background, detect_terminal_theme, osc_11_query, probe_background,
    probe_collect_replies, probe_query_batch,
};
pub use session::TerminalSession;
pub use sink::FrameSink;
pub use writer::{
    COALESCE_WINDOW, Coalescer, ReanchorCause, SettledBlock, SimulatedTxn, TransactionRecorder,
    Tui, Txn, paint_timer_read, paint_timer_reset, set_paint_timer,
};
