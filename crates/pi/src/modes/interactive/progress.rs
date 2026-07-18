//! Progress view-models: OAuth login progress, compaction/retry/bash progress,
//! and the pending (steering/follow-up) queue.
//!
//! These are pure data → pi-tui `Text`/`Loader` builders, decoupled from the
//! live session. The runtime feeds [`super::state`] snapshots; the composer
//! splices the built components above the editor.

use pi_tui::component::Component;
use pi_tui::components::{Loader, Spacer, Text};

use super::state::{
    AuthProgress, BashProgress, CompactionProgress, OAuthStage, PendingKind, PendingQueue,
    RetryProgress,
};
use super::theme::{self, ResolvedTheme, ThemeColor};

// ---------------------------------------------------------------------------
// Pending queue
// ---------------------------------------------------------------------------

/// Build the pending-messages component (steering + follow-up queue).
///
/// Renders one styled line per queued message, prefixed by kind.
#[must_use]
pub fn build_pending(queue: &PendingQueue, th: &ResolvedTheme) -> Box<dyn Component> {
    let mut stack = super::messages::ColumnStack::new();
    for msg in &queue.steering {
        stack.push(Box::new(Text::with_padding(
            pending_line(PendingKind::Steering, &msg.text, th),
            1,
            0,
        )));
    }
    for msg in &queue.follow_up {
        stack.push(Box::new(Text::with_padding(
            pending_line(PendingKind::FollowUp, &msg.text, th),
            1,
            0,
        )));
    }
    if !queue.follow_up.is_empty() {
        let mode = match queue.follow_up_mode {
            super::state::QueueMode::All => "all queued follow-ups will send after this turn",
            super::state::QueueMode::OneAtATime => "follow-ups send one at a time",
        };
        stack.push(Box::new(Text::with_padding(
            th.fg(ThemeColor::Dim, mode),
            1,
            0,
        )));
    }
    if stack.is_empty() {
        stack.push(Box::new(Spacer::new(0)));
    }
    Box::new(stack)
}

fn pending_line(kind: PendingKind, text: &str, th: &ResolvedTheme) -> String {
    let (glyph, label) = match kind {
        PendingKind::Steering => ("↳", "steer"),
        PendingKind::FollowUp => ("→", "queued"),
    };
    format!(
        "{} {}",
        th.fg(ThemeColor::Accent, glyph),
        th.fg(ThemeColor::Muted, &format!("{label}: ")),
    ) + text
}

// ---------------------------------------------------------------------------
// OAuth / auth progress
// ---------------------------------------------------------------------------

/// Build the auth-progress component (login dialog status line + spinner).
#[must_use]
pub fn build_auth_progress(progress: &AuthProgress, th: &ResolvedTheme) -> Box<dyn Component> {
    let mut loader = Loader::new(
        move |s: &str| theme::current().fg(ThemeColor::Accent, s),
        move |s: &str| theme::current().fg(ThemeColor::Muted, s),
        auth_stage_message(progress, th),
        None,
    );
    loader.set_frame_index(0);
    match progress.stage {
        OAuthStage::Failed => Box::new(Text::with_padding(
            th.fg(ThemeColor::Error, &auth_stage_message(progress, th)),
            1,
            0,
        )),
        OAuthStage::Done => Box::new(Text::with_padding(
            th.fg(ThemeColor::Success, &auth_stage_message(progress, th)),
            1,
            0,
        )),
        _ => Box::new(loader),
    }
}

/// The human-readable auth-stage message.
#[must_use]
pub fn auth_stage_message(progress: &AuthProgress, th: &ResolvedTheme) -> String {
    match progress.stage {
        OAuthStage::BrowserCallback => {
            let base = format!("Opening browser to log in to {}…", progress.provider);
            if let Some(url) = progress.detail.as_deref() {
                format!("{} {}", base, th.fg(ThemeColor::MdLink, url))
            } else {
                base
            }
        }
        OAuthStage::DeviceCode => {
            let base = format!("Device flow for {} — enter code:", progress.provider);
            if let Some(code) = progress.detail.as_deref() {
                format!("{base} {}", theme::bold(code))
            } else {
                base
            }
        }
        OAuthStage::ManualKey => format!("Enter API key for {}:", progress.provider),
        OAuthStage::Exchanging => format!("Exchanging token for {}…", progress.provider),
        OAuthStage::Done => format!("Logged in to {}.", progress.provider),
        OAuthStage::Failed => format!("Failed to log in to {}.", progress.provider),
    }
}

// ---------------------------------------------------------------------------
// Compaction / retry / bash progress
// ---------------------------------------------------------------------------

/// Build the compaction-progress component (a working status with reason text).
#[must_use]
pub fn build_compaction_progress(
    progress: &CompactionProgress,
    th: &ResolvedTheme,
) -> Box<dyn Component> {
    let msg = super::status::compaction_message(progress.reason);
    let accent = th.clone();
    let muted = th.clone();
    let mut loader = Loader::new(
        move |s: &str| accent.fg(ThemeColor::Accent, s),
        move |s: &str| muted.fg(ThemeColor::Muted, s),
        msg,
        None,
    );
    loader.set_frame_index(0);
    Box::new(loader)
}

/// Build the retry-progress component with a countdown message.
#[must_use]
pub fn build_retry_progress(progress: &RetryProgress, th: &ResolvedTheme) -> Box<dyn Component> {
    let msg =
        super::status::retry_message(progress.attempt, progress.max_attempts, progress.seconds);
    let warn = th.clone();
    let muted = th.clone();
    let mut loader = Loader::new(
        move |s: &str| warn.fg(ThemeColor::Warning, s),
        move |s: &str| muted.fg(ThemeColor::Muted, s),
        msg,
        None,
    );
    loader.set_frame_index(0);
    Box::new(loader)
}

/// Build the bash-progress component (live command + output preview).
#[must_use]
pub fn build_bash_progress(progress: &BashProgress, th: &ResolvedTheme) -> Box<dyn Component> {
    super::messages::build_bash(
        &super::messages::BashMessageView {
            command: progress.command.clone(),
            output: progress.output.clone(),
            expanded: progress.expanded,
            exit_code: progress.exit_code,
            cancelled: progress.cancelled,
            truncated: false,
            full_output_path: None,
        },
        th,
    )
}
