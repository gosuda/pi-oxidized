//! Status indicator view-model (working / retry / compaction / branch).
//!
//! Ports `.references/pi/packages/coding-agent/src/modes/interactive/components/status-indicator.ts`.
//! Wraps pi-tui's `Loader` braille spinner with a kind + message.

use pi_tui::component::Component;
use pi_tui::components::Loader;

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
    Box::new(loader)
}

/// The status message text (ports each indicator's label builder).
#[must_use]
pub fn status_message(status: &SessionStatus, th: &ResolvedTheme) -> String {
    let cancel = th.fg(ThemeColor::Dim, " · esc to cancel");
    match status.kind {
        StatusKind::Working => {
            let elapsed = if status.elapsed_secs == 0 {
                String::new()
            } else {
                format!(" {}s", status.elapsed_secs)
            };
            th.fg(
                ThemeColor::Muted,
                &format!("{}{elapsed}{cancel}", status.message),
            )
        }
        StatusKind::Retry => th.fg(ThemeColor::Muted, &format!("Retrying…{cancel}")),
        StatusKind::Compaction => {
            th.fg(ThemeColor::Muted, &format!("Compacting context…{cancel}"))
        }
        StatusKind::BranchSummary => {
            th.fg(ThemeColor::Muted, &format!("Summarizing branch…{cancel}"))
        }
    }
}

/// Build a retry-status view-model from attempt/countdown data.
#[must_use]
pub fn retry_message(attempt: u32, max_attempts: u32, seconds: u32) -> String {
    format!("Retrying ({attempt}/{max_attempts}) in {seconds}s… (Esc to cancel)")
}

/// Build a compaction-status view-model from a reason.
#[must_use]
pub fn compaction_message(reason: super::state::CompactionReason) -> String {
    match reason {
        super::state::CompactionReason::Manual => "Compacting context… (Esc to cancel)".to_owned(),
        super::state::CompactionReason::Threshold => "Auto-compacting… (Esc to cancel)".to_owned(),
        super::state::CompactionReason::Overflow => {
            "Context overflow detected, auto-compacting… (Esc to cancel)".to_owned()
        }
    }
}

/// Idle status renders two blank lines (ports `IdleStatus`).
#[must_use]
pub fn build_idle(width: u16) -> Box<dyn Component> {
    let _ = width;
    let mut stack = super::messages::ColumnStack::new();
    stack.push(Box::new(pi_tui::components::Spacer::new(1)));
    stack.push(Box::new(pi_tui::components::Spacer::new(1)));
    Box::new(stack)
}
