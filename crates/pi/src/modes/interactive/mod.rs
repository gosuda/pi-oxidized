//! Phase 4 interactive mode view-model.
//!
//! Stateless presentation layer for the interactive terminal product. Owns no
//! live agent-session handle and performs no session I/O. Consumes a pure data
//! snapshot ([`ViewState`]) and composes pi-tui components into a Ratatui
//! buffer in the exact order required by the reference interactive mode:
//!
//! ```text
//! header → resources → chat → pending → status
//!        → widgets above → editor → widgets below → footer
//! ```
//!
//! The runtime event loop (a later phase) owns a [`ViewState`], feeds
//! [`ViewAction`]s back into it, and calls [`compose`] / [`render_view`] to
//! paint. Everything here is terminal-free and stdout-free so it is fully
//! testable against golden buffer snapshots.

pub mod footer;
pub mod header;
pub mod input;
pub mod messages;
pub mod progress;
pub mod runtime;
pub mod selectors;
pub mod startup;
pub mod status;
pub mod theme;
pub mod tool_renderer;

mod state;
mod view;

pub use state::ViewState;
pub use view::{ComposedSection, ComposedView, compose, render_view, render_view_with_height};

pub use theme::{ColorMode, ResolvedTheme, ThemeBg, ThemeColor, ThemeError, ThemeJson};
pub use tool_renderer::{
    CustomToolRenderer, ToolCallView, ToolRenderError, ToolResultView, ToolState,
};

pub use input::{DoubleEscapeAction, InputMapper, InputState};
/// Re-export of the pi-tui component contract for downstream callers.
pub use pi_tui::component::{Component, EventResult, UiEvent};
pub use runtime::{
    BACKGROUND_COALESCE_WINDOW, DRAW_TIMEOUT, EVENT_CHANNEL_CAPACITY, EventSubscription,
    InteractiveExit, InteractiveRuntime, InteractiveRuntimeOptions, SessionHost, SessionSnapshot,
    SharedWriter, mock_input,
};

#[cfg(test)]
mod tests;
