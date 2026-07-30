//! Footer status-bar view-model.
//!
//! Ports `.references/pi/packages/coding-agent/src/modes/interactive/components/footer.ts`.
//! Pure data → pi-tui `Text` lines (cwd + stats + model). No session reads.

use pi_tui::component::Component;
use pi_tui::components::Text;
use pi_tui::text::{truncate_to_width, visible_width};

use super::state::{BillingMode, FooterData};
use super::theme::{self, ResolvedTheme, ThemeColor};

/// Format a token count for compact footer display (ports `formatTokens`).
#[must_use]
pub fn format_tokens(count: u64) -> String {
    if count < 1000 {
        return count.to_string();
    }
    if count < 10_000 {
        return format!(
            "{:.1}k",
            f64::from(u32::try_from(count).unwrap_or(0)) / 1000.0
        );
    }
    if count < 1_000_000 {
        return format!("{}k", count / 1000);
    }
    if count < 10_000_000 {
        // Integer tenths avoid u64→f64 precision loss for huge token counts.
        let tenths = count / 100_000; // count / 1e6 * 10
        return format!("{}.{}M", tenths / 10, tenths % 10);
    }
    format!("{}M", count / 1_000_000)
}

/// Collapse a home directory to `~` (ports `formatCwdForFooter`).
#[must_use]
pub fn format_cwd_for_footer(cwd: &str, home: &str) -> String {
    if home.is_empty() {
        return cwd.to_owned();
    }
    let resolved_cwd = normalize_path(cwd);
    let resolved_home = normalize_path(home);
    if resolved_cwd == resolved_home {
        return "~".to_owned();
    }
    if let Some(rest) = resolved_cwd.strip_prefix(&format!("{resolved_home}/")) {
        return format!("~/{rest}");
    }
    if let Some(rest) = resolved_cwd.strip_prefix(&format!("{resolved_home}\\")) {
        return format!("~\\{rest}");
    }
    cwd.to_owned()
}

fn normalize_path(p: &str) -> String {
    // Best-effort trailing-slash trim; full canonicalization is the runtime's job.
    let trimmed = p.trim_end_matches(['/', '\\']);
    if trimmed.is_empty() {
        p.to_owned()
    } else {
        trimmed.to_owned()
    }
}

/// Build the footer component (2-3 lines) from footer data at `width`.
#[must_use]
pub fn build_footer(
    data: &FooterData,
    theme_handle: &ResolvedTheme,
    width: u16,
) -> Box<dyn Component> {
    let lines = render_footer_lines(data, theme_handle, usize::from(width));
    let mut stack = super::messages::ColumnStack::new();
    for line in lines {
        stack.push(Box::new(Text::with_padding(line, 0, 0)));
    }
    Box::new(stack)
}

/// Render the footer text lines (joined; caller wraps in `Text`).
#[must_use]
pub fn render_footer_lines(data: &FooterData, th: &ResolvedTheme, width: usize) -> Vec<String> {
    let pwd_line = render_location_line(data, th, width);
    let segments = stats_segments(data, th);
    let model = model_status(data);
    let stats_line = compose_stats_line(segments, &model, th, width);
    let mut lines = vec![pwd_line, stats_line];

    if let Some(statuses) = extension_status_line(data, th) {
        lines.push(statuses);
    }
    lines
}

fn render_location_line(data: &FooterData, th: &ResolvedTheme, width: usize) -> String {
    let mut pwd = format_cwd_for_footer(&data.cwd, &data.home);
    if let Some(branch) = data.git_branch.as_deref() {
        pwd.push_str(" (");
        pwd.push_str(branch);
        pwd.push(')');
    }
    if let Some(name) = data.session_name.as_deref() {
        pwd.push_str(" • ");
        pwd.push_str(name);
    }
    truncate_to_width(
        &th.fg(ThemeColor::Dim, &pwd),
        width,
        &th.fg(ThemeColor::Dim, "..."),
        false,
    )
}

/// Build the left-side stats segments as `(keep-priority, text)` pairs.
///
/// Higher priority survives longer when `compose_stats_line` drops segments to
/// fit `width`; the raw R/W counters stay in `FooterData` because the hit rate
/// and HTML export still consume them.
fn stats_segments(data: &FooterData, th: &ResolvedTheme) -> Vec<(u8, String)> {
    let mut parts: Vec<(u8, String)> = Vec::new();
    if data.total_input > 0 {
        parts.push((1, format!("{} in", format_tokens(data.total_input))));
    }
    if data.total_output > 0 {
        parts.push((2, format!("{} out", format_tokens(data.total_output))));
    }
    if let Some(rate) = data
        .cache_hit_rate
        .filter(|_| data.total_cache_read > 0 || data.total_cache_write > 0)
    {
        parts.push((0, format!("{rate:.0}% cached")));
    }
    let subscription = data.flags.billing == BillingMode::Subscription;
    if data.total_cost > 0.0 || subscription {
        let suffix = if subscription { " (sub)" } else { "" };
        parts.push((3, format!("${:.3}{suffix}", data.total_cost)));
    }

    parts.push((4, context_status(data, th)));
    if data.flags.experimental {
        parts.push((
            1,
            format!(
                "{} {}",
                th.fg(ThemeColor::Dim, "•"),
                theme::bold(&th.fg(ThemeColor::Warning, "xp"))
            ),
        ));
    }
    parts
}

fn context_status(data: &FooterData, th: &ResolvedTheme) -> String {
    let auto_indicator = if data.flags.auto_compact {
        " (auto)"
    } else {
        ""
    };
    let context_display = data.context_percent.map_or_else(
        || {
            format!(
                "? of {}{}",
                format_tokens(data.context_window),
                auto_indicator
            )
        },
        |pct| {
            format!(
                "{pct:.0}% of {}{}",
                format_tokens(data.context_window),
                auto_indicator
            )
        },
    );
    match data.context_percent.unwrap_or(0.0) {
        pct if pct > 90.0 => th.fg(ThemeColor::Error, &context_display),
        pct if pct > 70.0 => th.fg(ThemeColor::Warning, &context_display),
        _ => context_display,
    }
}

fn model_status(data: &FooterData) -> String {
    let model_name = if data.model_id.is_empty() {
        "no-model"
    } else {
        &data.model_id
    };
    let status = if data.flags.reasoning {
        let level = thinking_label(data.thinking_level);
        if data.thinking_level == pi_ai::ModelThinkingLevel::Off {
            format!("{model_name} • thinking off")
        } else {
            format!("{model_name} • {level}")
        }
    } else {
        model_name.to_owned()
    };
    if data.provider_count > 1
        && let Some(provider) = data.provider.as_deref()
    {
        format!("({provider}) {status}")
    } else {
        status
    }
}

fn extension_status_line(data: &FooterData, th: &ResolvedTheme) -> Option<String> {
    if data.extension_statuses.is_empty() {
        return None;
    }
    let statuses = data
        .extension_statuses
        .values()
        .cloned()
        .collect::<Vec<_>>()
        .join(" ");
    Some(th.fg(ThemeColor::Dim, &statuses))
}

/// Compose the two-column stats line (left stats, right-aligned model).
///
/// Drops whole left segments by ascending keep-priority until the row fits;
/// the right side is never dropped — it is ellipsis-truncated only when it
/// alone exceeds `width`.
fn compose_stats_line(
    mut segments: Vec<(u8, String)>,
    right_side: &str,
    th: &ResolvedTheme,
    width: usize,
) -> String {
    let join_left = |parts: &[(u8, String)]| {
        parts
            .iter()
            .map(|(_, text)| text.as_str())
            .collect::<Vec<_>>()
            .join(" · ")
    };
    let right_width = visible_width(right_side);
    let left_fits = |parts: &[(u8, String)]| {
        let left_width = parts
            .iter()
            .map(|(_, text)| visible_width(text))
            .sum::<usize>()
            + parts.len().saturating_sub(1) * 3;
        left_width + 2 + right_width <= width
    };
    while segments.len() > 1 && !left_fits(&segments) {
        let Some((drop_idx, _)) = segments
            .iter()
            .enumerate()
            .min_by_key(|(_, (priority, _))| *priority)
        else {
            break;
        };
        segments.remove(drop_idx);
    }
    let left = join_left(&segments);
    let left_width = visible_width(&left);
    if left_width + 2 + right_width <= width {
        let padding = width.saturating_sub(left_width).saturating_sub(right_width);
        let pad = " ".repeat(padding);
        let left_dim = th.fg(ThemeColor::Dim, &left);
        let right_dim = th.fg(ThemeColor::Dim, &format!("{pad}{right_side}"));
        return format!("{left_dim}{right_dim}");
    }
    // Right side is never dropped; it is ellipsis-truncated when it (or the
    // remaining single left segment) still overflows.
    let ellipsis = th.fg(ThemeColor::Dim, "…");
    let truncated = truncate_to_width(right_side, width, &ellipsis, false);
    th.fg(ThemeColor::Dim, &truncated)
}

/// Lowercase thinking-level label.
fn thinking_label(level: pi_ai::ModelThinkingLevel) -> &'static str {
    match level {
        pi_ai::ModelThinkingLevel::Off => "off",
        pi_ai::ModelThinkingLevel::Minimal => "minimal",
        pi_ai::ModelThinkingLevel::Low => "low",
        pi_ai::ModelThinkingLevel::Medium => "medium",
        pi_ai::ModelThinkingLevel::High => "high",
        pi_ai::ModelThinkingLevel::Xhigh => "xhigh",
        pi_ai::ModelThinkingLevel::Max => "max",
    }
}

#[cfg(test)]
mod tests {
    use pi_tui::components::util::strip_ansi;
    use pi_tui::text::visible_width;

    use super::*;
    use crate::modes::interactive::state::{FooterData, FooterFlags};
    use crate::modes::interactive::theme;

    /// Verification §3 footer-ladder: at width 40 with every counter
    /// populated, the lowest-priority `cached` segment is dropped while the
    /// model segment survives intact on the right.
    #[test]
    fn footer_ladder_drops_cached_keeps_model() {
        let data = FooterData {
            total_input: 1234,
            total_output: 567,
            total_cache_read: 8900,
            total_cache_write: 1000,
            cache_hit_rate: Some(89.9),
            total_cost: 0.012,
            context_window: 200_000,
            context_percent: Some(42.5),
            model_id: "claude-sonnet".to_owned(),
            thinking_level: pi_ai::ModelThinkingLevel::Medium,
            flags: FooterFlags {
                reasoning: true,
                ..FooterFlags::default()
            },
            ..FooterData::default()
        };
        let th = theme::dark();
        let lines = render_footer_lines(&data, &th, 40);
        let stats = strip_ansi(&lines[1]);
        assert!(
            !stats.contains("cached"),
            "width 40 must drop the cached segment, got: {stats:?}"
        );
        assert!(
            stats.contains("claude-sonnet"),
            "model segment must survive intact, got: {stats:?}"
        );
        assert!(
            stats.contains("medium"),
            "thinking label must survive intact, got: {stats:?}"
        );
        assert!(
            visible_width(&stats) <= 40,
            "line must fit width 40, got: {stats:?}"
        );
    }

    /// The right side is never dropped: when the model segment alone exceeds
    /// the width it is ellipsis-truncated rather than omitted.
    #[test]
    fn footer_ladder_truncates_right_with_ellipsis() {
        let data = FooterData {
            context_window: 200_000,
            context_percent: Some(42.5),
            model_id: "a-very-long-model-identifier-that-overflows".to_owned(),
            flags: FooterFlags::default(),
            ..FooterData::default()
        };
        let th = theme::dark();
        let lines = render_footer_lines(&data, &th, 12);
        let stats = strip_ansi(&lines[1]);
        assert!(
            stats.contains('…'),
            "overflowing right side must be ellipsis-truncated, got: {stats:?}"
        );
        assert!(
            visible_width(&stats) <= 12,
            "line must fit width 12, got: {stats:?}"
        );
    }
}
