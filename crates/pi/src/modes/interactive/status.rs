//! Status indicator view-model (working / retry / compaction / branch).
//!
//! Ports `.references/pi/packages/coding-agent/src/modes/interactive/components/status-indicator.ts`.
//! Wraps pi-tui's `Loader` braille spinner with a kind + message.

use pi_tui::component::Component;
use pi_tui::components::{Loader, Padded};

use super::state::{SessionStatus, StatusKind};
use super::theme::{self, ResolvedTheme, ThemeColor};

/// Build a status-indicator loader component for the given status.
#[must_use]
pub fn build_status(status: &SessionStatus, th: &ResolvedTheme) -> Box<dyn Component> {
    let spinner_color = match status.kind {
        StatusKind::Working | StatusKind::Compaction | StatusKind::BranchSummary => {
            ThemeColor::Accent
        }
        StatusKind::Retry => ThemeColor::Warning,
    };
    let spinner_fn = move |s: &str| theme::current().fg(spinner_color, s);
    let message_fn = move |s: &str| theme::current().fg(ThemeColor::Muted, s);
    let mut loader = Loader::new(spinner_fn, message_fn, status_message(status, th), None);
    loader.set_frame_index(status.frame);
    // Loader self-indents one column inside pi-tui; the product adds the
    // missing column so the status line shares the column-2 left edge.
    let mut padded = Padded::with_padding(1, 0);
    padded.add_child(loader);
    Box::new(padded)
}

/// The status message text (ports each indicator's label builder).
#[must_use]
pub fn status_message(status: &SessionStatus, th: &ResolvedTheme) -> String {
    let cancel = th.fg(
        ThemeColor::Dim,
        &format!(
            " · {} to cancel",
            pi_tui::keybindings::key_text("app.interrupt")
        ),
    );
    let elapsed = if status.elapsed_secs == 0 {
        String::new()
    } else {
        format!(" {}s", status.elapsed_secs)
    };
    match status.kind {
        StatusKind::Working => th.fg(
            ThemeColor::Muted,
            &format!("{}{elapsed}{cancel}", status.message),
        ),
        StatusKind::Retry => th.fg(ThemeColor::Muted, &format!("Retrying…{elapsed}{cancel}")),
        StatusKind::Compaction => th.fg(
            ThemeColor::Muted,
            &format!("Compacting context…{elapsed}{cancel}"),
        ),
        StatusKind::BranchSummary => th.fg(
            ThemeColor::Muted,
            &format!("Summarizing branch…{elapsed}{cancel}"),
        ),
    }
}

/// Build a retry-status view-model from attempt/countdown data.
#[must_use]
pub fn retry_message(attempt: u32, max_attempts: u32, seconds: u32) -> String {
    format!(
        "Retrying ({attempt}/{max_attempts}) in {seconds}s… ({} to cancel)",
        pi_tui::keybindings::key_text("app.interrupt")
    )
}

/// Build a compaction-status view-model from a reason.
#[must_use]
pub fn compaction_message(reason: super::state::CompactionReason) -> String {
    let cancel = format!(
        "({} to cancel)",
        pi_tui::keybindings::key_text("app.interrupt")
    );
    match reason {
        super::state::CompactionReason::Manual => format!("Compacting context… {cancel}"),
        super::state::CompactionReason::Threshold => format!("Auto-compacting… {cancel}"),
        super::state::CompactionReason::Overflow => {
            format!("Context overflow detected, auto-compacting… {cancel}")
        }
    }
}

/// Idle status renders one blank line above the editor (D4 already leaves a
/// blank row after the last turn; two more would read as a gap, not grouping).
#[must_use]
pub fn build_idle(width: u16) -> Box<dyn Component> {
    let _ = width;
    let mut stack = super::messages::ColumnStack::new();
    stack.push(Box::new(pi_tui::components::Spacer::new(1)));
    Box::new(stack)
}
