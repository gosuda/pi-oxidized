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
    let stats = token_status(data, th);
    let model = model_status(data);
    let stats_line = compose_stats_line(&stats, &model, th, width);
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

fn token_status(data: &FooterData, th: &ResolvedTheme) -> String {
    let mut parts = Vec::new();
    if data.total_input > 0 {
        parts.push(format!("↑{}", format_tokens(data.total_input)));
    }
    if data.total_output > 0 {
        parts.push(format!("↓{}", format_tokens(data.total_output)));
    }
    if data.total_cache_read > 0 {
        parts.push(format!("R{}", format_tokens(data.total_cache_read)));
    }
    if data.total_cache_write > 0 {
        parts.push(format!("W{}", format_tokens(data.total_cache_write)));
    }
    if let Some(rate) = data
        .cache_hit_rate
        .filter(|_| data.total_cache_read > 0 || data.total_cache_write > 0)
    {
        parts.push(format!("CH{rate:.1}%"));
    }
    let subscription = data.flags.billing == BillingMode::Subscription;
    if data.total_cost > 0.0 || subscription {
        let suffix = if subscription { " (sub)" } else { "" };
        parts.push(format!("${:.3}{suffix}", data.total_cost));
    }

    parts.push(context_status(data, th));
    if data.flags.experimental {
        parts.push(format!(
            "{} {}",
            th.fg(ThemeColor::Dim, "•"),
            theme::bold(&th.fg(ThemeColor::Warning, "xp"))
        ));
    }
    parts.join(" ")
}

fn context_status(data: &FooterData, th: &ResolvedTheme) -> String {
    let auto_indicator = if data.flags.auto_compact {
        " (auto)"
    } else {
        ""
    };
    let context_display = data.context_percent.map_or_else(
        || format!("?/{}{}", format_tokens(data.context_window), auto_indicator),
        |pct| {
            format!(
                "{pct:.1}%/{}{}",
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
fn compose_stats_line(
    stats_left: &str,
    right_side: &str,
    th: &ResolvedTheme,
    width: usize,
) -> String {
    let stats_width = visible_width(stats_left);
    let right_width = visible_width(right_side);
    let total_needed = stats_width.saturating_add(2).saturating_add(right_width);
    if total_needed <= width {
        let padding = width
            .saturating_sub(stats_width)
            .saturating_sub(right_width);
        let pad = " ".repeat(padding);
        let left_dim = th.fg(ThemeColor::Dim, stats_left);
        let right_dim = th.fg(ThemeColor::Dim, &format!("{pad}{right_side}"));
        format!("{left_dim}{right_dim}")
    } else {
        let avail_for_right = width.saturating_sub(stats_width).saturating_sub(2);
        if avail_for_right > 0 {
            let truncated = truncate_to_width(right_side, avail_for_right, "", false);
            let pad = " ".repeat(
                width
                    .saturating_sub(stats_width)
                    .saturating_sub(visible_width(&truncated)),
            );
            let left_dim = th.fg(ThemeColor::Dim, stats_left);
            let right_dim = th.fg(ThemeColor::Dim, &format!("{pad}{truncated}"));
            format!("{left_dim}{right_dim}")
        } else {
            th.fg(ThemeColor::Dim, stats_left)
        }
    }
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
