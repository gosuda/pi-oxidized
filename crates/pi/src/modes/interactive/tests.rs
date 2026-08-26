//! Golden snapshot tests for the interactive view-model.
//!
//! Renders the composed view into Ratatui buffers at widths 20/80/160 across
//! the required states (empty/loading/error/streaming/compact/retry/bash/
//! queue), focus/overlay/selector scenarios, resize, theme-invalid fallback,
//! and message/tool content. Produces both plain-text cell dumps and ANSI
//! snapshots. **No terminal, no stdout** — pure buffer assertions.

use std::fmt::Write as _;

use pi_ai::{AssistantContent, AssistantMessage, StopReason, TextContent, ThinkingContent};

use crate::modes::interactive::footer::{format_cwd_for_footer, format_tokens};
use crate::modes::interactive::messages::{
    AssistantMessageView, BashMessageView, BranchSummaryView, CompactionSummaryView,
    CustomMessageView, MessageView as StateMessageView, SkillInvocationView, UserMessageView,
};
use crate::modes::interactive::state::{
    AuthProgress, BashProgress, BillingMode, CompactionProgress, CompactionReason, FooterData,
    FooterFlags, HeaderData, LoadedResource, OAuthStage, Overlay, OverlayKind, PendingKind,
    PendingMessage, PendingQueue, QueueMode, RetryProgress, SessionStatus, ShortcutHint,
    StartupDiagnostic, StatusKind, ViewState, WidgetSlot,
};
use crate::modes::interactive::theme::{self, ColorMode, ThemeColor};
use crate::modes::interactive::view::{
    compose, render_component, render_view, snapshot_buffer_ansi, snapshot_buffer_plain,
};

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Render `state` at three widths and return the joined plain snapshots.
fn triple_plain(state: &ViewState) -> String {
    let mut out = String::new();
    for &w in &[20_u16, 80, 160] {
        let buf = render_view(state, w, 60);
        let rows = snapshot_buffer_plain(&buf, w, 60);
        let _ = writeln!(out, "===== width {w} =====");
        for row in &rows {
            out.push_str(row);
            out.push('\n');
        }
        out.push('\n');
    }
    out
}

fn assistant_text(text: &str) -> AssistantMessage {
    let mut msg = AssistantMessage::new("anthropic-messages", "anthropic", "claude-test", 0);
    msg.content
        .push(AssistantContent::Text(TextContent::new(text)));
    msg
}

fn assistant_with_thinking(thinking: &str, text: &str) -> AssistantMessage {
    let mut msg = assistant_text(text);
    msg.content.insert(
        0,
        AssistantContent::Thinking(ThinkingContent::new(thinking)),
    );
    msg
}

fn sample_footer() -> FooterData {
    FooterData {
        cwd: "/home/user/projects/pi".to_owned(),
        home: "/home/user".to_owned(),
        git_branch: Some("main".to_owned()),
        session_name: Some("refactor".to_owned()),
        total_input: 1234,
        total_output: 567,
        total_cache_read: 8900,
        total_cache_write: 1000,
        cache_hit_rate: Some(89.9),
        total_cost: 0.012,
        context_window: 200_000,
        context_percent: Some(42.5),
        model_id: "claude-sonnet".to_owned(),
        provider: None,
        provider_count: 1,
        thinking_level: pi_ai::ModelThinkingLevel::Medium,
        flags: FooterFlags {
            billing: BillingMode::Metered,
            auto_compact: true,
            reasoning: true,
            experimental: false,
        },
        extension_statuses: std::collections::BTreeMap::default(),
    }
}

fn base_state() -> ViewState {
    let mut s = ViewState::empty();
    s.header = HeaderData {
        app_name: "pi".to_owned(),
        version: "0.1.0".to_owned(),
        expanded: false,
        onboarding: Some("type a message to begin".to_owned()),
    };
    s.footer = sample_footer();
    s
}

// ---------------------------------------------------------------------------
// Composition order
// ---------------------------------------------------------------------------

#[test]
fn empty_state_renders_canonical_section_order() {
    let state = base_state();
    let view = compose(&state);
    let actual: Vec<&str> = view.sections.iter().map(|s| s.label).collect();
    let canonical = [
        "header",
        "resources",
        "diagnostics",
        "chat",
        "pending",
        "status",
        "widgets-above",
        "editor",
        "widgets-below",
        "footer",
    ];
    assert_eq!(
        actual, canonical,
        "section order must match the reference composition"
    );
}

// ---------------------------------------------------------------------------
// State snapshots at 20/80/160
// ---------------------------------------------------------------------------

#[test]
fn empty_state_snapshot_widths() {
    crate::core::keybindings::with_global_app_keybindings(|| {
        let state = base_state();
        insta::assert_snapshot!("empty_state_widths", triple_plain(&state));
    });
}

#[test]
fn loading_state_snapshot() {
    crate::core::keybindings::with_global_app_keybindings(|| {
        let mut state = base_state();
        state.status = Some(SessionStatus {
            kind: StatusKind::Working,
            frame: 0,
            elapsed_secs: 0,
            message: "Working…".to_owned(),
        });
        insta::assert_snapshot!("loading_state_widths", triple_plain(&state));
    });
}

#[test]
fn error_state_snapshot() {
    let mut state = base_state();
    let mut msg = assistant_text("partial response before failure");
    msg.stop_reason = StopReason::Error;
    msg.error_message = Some("provider returned 500".to_owned());
    state
        .messages
        .push(StateMessageView::Assistant(AssistantMessageView {
            message: msg,
            hide_thinking: false,
            hidden_thinking_label: "Thinking…".to_owned(),
            streaming: false,
        }));
    insta::assert_snapshot!("error_state_widths", triple_plain(&state));
}

#[test]
fn streaming_state_snapshot() {
    crate::core::keybindings::with_global_app_keybindings(|| {
        let mut state = base_state();
        state.streaming = true;
        state.status = Some(SessionStatus {
            kind: StatusKind::Working,
            frame: 3,
            elapsed_secs: 0,
            message: "Working…".to_owned(),
        });
        let partial = assistant_text("Generating a response…");
        state
            .messages
            .push(StateMessageView::streaming_assistant(partial));
        insta::assert_snapshot!("streaming_state_widths", triple_plain(&state));
    });
}

#[test]
fn compact_state_snapshot() {
    crate::core::keybindings::with_global_app_keybindings(|| {
        let mut state = base_state();
        state.status = Some(SessionStatus {
            kind: StatusKind::Compaction,
            frame: 0,
            elapsed_secs: 0,
            message: "Compacting context…".to_owned(),
        });
        state
            .messages
            .push(StateMessageView::Compaction(CompactionSummaryView {
                summary: "Earlier discussion covered the theme system and footer layout."
                    .to_owned(),
                tokens_before: 180_000,
            }));
        insta::assert_snapshot!("compact_state_widths", triple_plain(&state));
    });
}

#[test]
fn retry_state_snapshot() {
    crate::core::keybindings::with_global_app_keybindings(|| {
        let mut state = base_state();
        state.status = Some(SessionStatus {
            kind: StatusKind::Retry,
            frame: 5,
            elapsed_secs: 0,
            message: "Retrying…".to_owned(),
        });
        insta::assert_snapshot!("retry_state_widths", triple_plain(&state));
    });
}

#[test]
fn status_spinner_frame_and_elapsed_render() {
    let mut state = base_state();
    state.status = Some(SessionStatus {
        kind: StatusKind::Working,
        frame: 3,
        elapsed_secs: 12,
        message: "Working…".to_owned(),
    });
    let buf = render_view(&state, 80, 60);
    let plain = snapshot_buffer_plain(&buf, 80, 60).join("\n");
    assert!(
        plain.contains(pi_tui::components::DEFAULT_LOADER_FRAMES[3]),
        "status line must render spinner frame 3 (⠸): {plain}"
    );
    assert!(
        plain.contains("12s"),
        "status line must show the elapsed-seconds counter: {plain}"
    );
}

#[test]
fn bash_state_snapshot() {
    let mut state = base_state();
    state.messages.push(StateMessageView::Bash(BashMessageView {
        command: "cargo build --release".to_owned(),
        output: "   Compiling pi v0.1.0\n    Finished release".to_owned(),
        expanded: false,
        exit_code: Some(0),
        cancelled: false,
        truncated: false,
        full_output_path: None,
    }));
    insta::assert_snapshot!("bash_state_widths", triple_plain(&state));
}

#[test]
fn queue_state_snapshot() {
    crate::core::keybindings::with_global_app_keybindings(|| {
        let mut state = base_state();
        state.streaming = true;
        state.status = Some(SessionStatus {
            kind: StatusKind::Working,
            frame: 0,
            elapsed_secs: 0,
            message: "Working…".to_owned(),
        });
        state.pending = PendingQueue {
            steering: vec![PendingMessage {
                kind: PendingKind::Steering,
                text: "remember to add tests".to_owned(),
            }],
            follow_up: vec![
                PendingMessage {
                    kind: PendingKind::FollowUp,
                    text: "now run clippy".to_owned(),
                },
                PendingMessage {
                    kind: PendingKind::FollowUp,
                    text: "then commit".to_owned(),
                },
            ],
            follow_up_mode: QueueMode::All,
        };
        insta::assert_snapshot!("queue_state_widths", triple_plain(&state));
    });
}

// ---------------------------------------------------------------------------
// Rail + shared left edge (Step 2)
// ---------------------------------------------------------------------------

#[test]
fn rail_not_slab_for_user_block() {
    let mut state = base_state();
    state.messages.push(StateMessageView::User(UserMessageView {
        text: "hello rail".to_owned(),
    }));
    let buf = render_view(&state, 80, 30);
    let rows = snapshot_buffer_plain(&buf, 80, 30);
    let rail_rows: Vec<usize> = rows
        .iter()
        .enumerate()
        .filter_map(|(i, row)| row.starts_with('│').then_some(i))
        .collect();
    assert!(
        !rail_rows.is_empty(),
        "user block must draw the rail glyph at column 0: {rows:?}"
    );
    assert!(
        rail_rows.iter().all(|&i| rows[i].contains("hello rail")),
        "every railed row must carry user content: {rows:?}"
    );

    // No background slab: the rail rows must carry no non-default cell
    // background, so their ANSI snapshot contains no background SGR.
    let ansi = snapshot_buffer_ansi(&buf, 80, 30, ColorMode::Truecolor);
    for i in rail_rows {
        assert!(
            !ansi[i].contains("\x1b[48;"),
            "user block row must not paint a background (UserMessageBg slab): {:?}",
            ansi[i]
        );
    }
}

#[test]
fn shared_left_edge_at_column_two() {
    let mut state = base_state();
    state.resources.push(LoadedResource {
        kind: "skill".to_owned(),
        label: "commit".to_owned(),
    });
    state.messages.push(StateMessageView::User(UserMessageView {
        text: "user turn".to_owned(),
    }));
    state
        .messages
        .push(StateMessageView::Assistant(AssistantMessageView {
            message: assistant_text("assistant prose"),
            hide_thinking: false,
            hidden_thinking_label: "Thinking…".to_owned(),
            streaming: false,
        }));
    state.pending = PendingQueue {
        steering: vec![PendingMessage {
            kind: PendingKind::Steering,
            text: "pending steer".to_owned(),
        }],
        follow_up: Vec::new(),
        follow_up_mode: QueueMode::All,
    };
    let buf = render_view(&state, 80, 30);
    let rows = snapshot_buffer_plain(&buf, 80, 30);
    // D3 exempts the bottom chrome: the editor keeps its 1-column padding
    // (Step 4) and footer lines 1-2 keep their column-0 layout (Step 6).
    let non_blank: Vec<usize> = (0..rows.len())
        .filter(|&i| !rows[i].trim().is_empty())
        .collect();
    let transcript = &non_blank[..non_blank.len().saturating_sub(3)];
    let off_edge: Vec<(usize, &str)> = transcript
        .iter()
        .map(|&i| (i, rows[i].as_str()))
        .filter(|(_, row)| {
            let railed = row.starts_with("│ ") || row.starts_with("┃ ");
            let indented = row.starts_with("  ") && !row.starts_with("   ");
            !railed && !indented
        })
        .collect();
    assert!(
        off_edge.is_empty(),
        "every transcript row must start at the shared column-2 edge, via rail or indent (D3): {off_edge:?}"
    );
}

// ---------------------------------------------------------------------------
// Message + tool content
// ---------------------------------------------------------------------------

#[test]
fn user_and_assistant_messages_snapshot() {
    let mut state = base_state();
    state.messages.push(StateMessageView::User(UserMessageView {
        text: "Hello! Can you **explain** this code?".to_owned(),
    }));
    state
        .messages
        .push(StateMessageView::Assistant(AssistantMessageView {
            message: assistant_with_thinking(
                "Let me consider the structure.",
                "Here is the explanation:\n\n- point one\n- point two",
            ),
            hide_thinking: false,
            hidden_thinking_label: "Thinking…".to_owned(),
            streaming: false,
        }));
    insta::assert_snapshot!("user_assistant_messages", triple_plain(&state));
}

#[test]
fn hidden_thinking_renders_label() {
    let mut state = base_state();
    state
        .messages
        .push(StateMessageView::Assistant(AssistantMessageView {
            message: assistant_with_thinking("hidden reasoning", "visible answer"),
            hide_thinking: true,
            hidden_thinking_label: "Thinking…".to_owned(),
            streaming: false,
        }));
    let buf = render_view(&state, 80, 40);
    let plain = snapshot_buffer_plain(&buf, 80, 40).join("\n");
    assert!(
        plain.contains("Thinking…"),
        "hidden thinking label must render: {plain}"
    );
    assert!(
        !plain.contains("hidden reasoning"),
        "raw thinking must be hidden"
    );
}

#[test]
fn custom_branch_skill_messages_snapshot() {
    let mut state = base_state();
    state
        .messages
        .push(StateMessageView::Custom(CustomMessageView {
            custom_type: "note".to_owned(),
            text: "Extension-injected context".to_owned(),
        }));
    state
        .messages
        .push(StateMessageView::Branch(BranchSummaryView {
            summary: "Returned from an experimental branch.".to_owned(),
            from_id: "abc123".to_owned(),
        }));
    state
        .messages
        .push(StateMessageView::Skill(SkillInvocationView {
            name: "commit".to_owned(),
            text: "Invoked commit skill".to_owned(),
        }));
    insta::assert_snapshot!("custom_branch_skill_messages", triple_plain(&state));
}

#[test]
fn length_stop_reason_renders_error() {
    let mut state = base_state();
    let mut msg = assistant_text("partial");
    msg.stop_reason = StopReason::Length;
    state
        .messages
        .push(StateMessageView::Assistant(AssistantMessageView {
            message: msg,
            hide_thinking: false,
            hidden_thinking_label: "Thinking…".to_owned(),
            streaming: false,
        }));
    let buf = render_view(&state, 80, 30);
    let plain = snapshot_buffer_plain(&buf, 80, 30).join("\n");
    assert!(
        plain.contains("Response was truncated before completion"),
        "length stop reason must surface error: {plain}"
    );
}

#[test]
fn tool_message_renders_call_and_result() {
    use crate::modes::interactive::messages::ToolMessageView;
    use crate::modes::interactive::tool_renderer::{
        ToolCallView, ToolPhase, ToolResultView, ToolState,
    };
    let mut state = base_state();
    state.messages.push(StateMessageView::Tool(ToolMessageView {
        renderer: "read".to_owned(),
        state: ToolState {
            call: ToolCallView {
                name: "read".to_owned(),
                id: "call_1".to_owned(),
                args_summary: "path: src/lib.rs".to_owned(),
                raw_args: serde_json::json!({"path": "src/lib.rs"}),
            },
            result: Some(ToolResultView {
                text: "pub fn main() {}".to_owned(),
                truncated: false,
                full_output_path: None,
                images: Vec::new(),
                error: None,
            }),
            expanded: false,
            phase: ToolPhase::Success,
        },
    }));
    let buf = render_view(&state, 80, 30);
    let plain = snapshot_buffer_plain(&buf, 80, 30).join("\n");
    assert!(plain.contains("read"), "tool name must render: {plain}");
    assert!(
        plain.contains("pub fn main"),
        "tool result body must render: {plain}"
    );
}

#[test]
fn tool_signature_not_json_for_builtin_tools() {
    use crate::modes::interactive::messages::ToolMessageView;
    use crate::modes::interactive::tool_renderer::{
        ToolCallView, ToolPhase, ToolResultView, ToolState,
    };
    let mut state = base_state();
    state.messages.push(StateMessageView::Tool(ToolMessageView {
        renderer: "edit".to_owned(),
        state: ToolState {
            call: ToolCallView {
                name: "edit".to_owned(),
                id: "call_1".to_owned(),
                args_summary: "path: src/main.rs".to_owned(),
                raw_args: serde_json::json!({"path": "src/main.rs", "old": "a", "new": "b"}),
            },
            result: Some(ToolResultView {
                text: "Successfully replaced 1 block(s) in src/main.rs.".to_owned(),
                truncated: false,
                full_output_path: None,
                images: Vec::new(),
                error: None,
            }),
            expanded: false,
            phase: ToolPhase::Success,
        },
    }));
    let buf = render_view(&state, 80, 30);
    let plain = snapshot_buffer_plain(&buf, 80, 30).join("\n");
    assert!(
        plain.contains("edit src/main.rs"),
        "built-in renderer must print a typed signature: {plain}"
    );
    assert!(
        !plain.contains("\"path\":"),
        "raw JSON args must not appear: {plain}"
    );
}

#[test]
fn tool_error_uses_heavy_rail_glyph() {
    use crate::modes::interactive::messages::ToolMessageView;
    use crate::modes::interactive::tool_renderer::{
        ToolCallView, ToolPhase, ToolResultView, ToolState,
    };
    let mut state = base_state();
    state.messages.push(StateMessageView::Tool(ToolMessageView {
        renderer: "bash".to_owned(),
        state: ToolState {
            call: ToolCallView {
                name: "bash".to_owned(),
                id: "call_1".to_owned(),
                args_summary: "command: false".to_owned(),
                raw_args: serde_json::json!({"command": "false"}),
            },
            result: Some(ToolResultView {
                text: "exit status 1".to_owned(),
                truncated: false,
                full_output_path: None,
                images: Vec::new(),
                error: Some("exit status 1".to_owned()),
            }),
            expanded: false,
            phase: ToolPhase::Error,
        },
    }));
    let buf = render_view(&state, 80, 30);
    let rows = snapshot_buffer_plain(&buf, 80, 30);
    assert!(
        rows.iter().any(|row| row.starts_with("┃ ")),
        "an errored tool block must carry the heavy rail glyph (D5): {rows:?}"
    );
}

// ---------------------------------------------------------------------------
// Focus / overlay / selector
// ---------------------------------------------------------------------------

#[test]
fn shortcut_overlay_snapshot() {
    crate::core::keybindings::with_global_app_keybindings(|| {
        let mut state = base_state();
        state.overlay = Some(Overlay {
            kind: OverlayKind::ShortcutHelp,
            lines: Vec::new(),
            height: 20,
        });
        let buf = render_view(&state, 80, 40);
        let plain = snapshot_buffer_plain(&buf, 80, 40).join("\n");
        assert!(
            plain.contains("Keyboard shortcuts"),
            "shortcut overlay must render: {plain}"
        );
        assert!(plain.contains("Escape"));
    });
}

#[test]
fn shortcut_overlay_shows_extensions_only_when_present() {
    crate::core::keybindings::with_global_app_keybindings(|| {
        let mut state = base_state();
        state.overlay = Some(Overlay {
            kind: OverlayKind::ShortcutHelp,
            lines: Vec::new(),
            height: 20,
        });
        let plain = snapshot_buffer_plain(&render_view(&state, 80, 40), 80, 40).join("\n");
        assert!(!plain.contains("Extensions"));

        state.extension_shortcuts.push(ShortcutHint {
            key: "ctrl+y".to_owned(),
            action: "Extension action".to_owned(),
        });
        let plain = snapshot_buffer_plain(&render_view(&state, 80, 40), 80, 40).join("\n");
        assert!(plain.contains("Extensions"));
        assert!(plain.contains("Ctrl+Y"));
        assert!(plain.contains("Extension action"));
    });
}

#[test]
fn extension_overlay_uses_resolved_anchor_width_and_margin() {
    let mut state = base_state();
    state.width = 40;
    state.height = 12;
    state.overlay = Some(Overlay {
        kind: OverlayKind::Extension,
        lines: Vec::new(),
        height: 1,
    });
    state.extension_overlay_slot =
        Some(pi_ext::sanitize::sanitize_slot(&pi_ext::protocol::UiSlot {
            key: "geometry".to_owned(),
            generation: 1,
            placement: pi_ext::protocol::SlotPlacement::Overlay,
            height: 1,
            runs: vec![vec![pi_ext::protocol::StyledRun {
                text: "GEOM".to_owned(),
                style: pi_ext::protocol::Style::default(),
            }]],
            focusable: false,
            cursor: None,
            overlay_options: Some(pi_ext::protocol::OverlaySpec {
                width: Some(pi_ext::protocol::SizeValue::Cells(12)),
                anchor: Some(pi_ext::protocol::OverlayAnchor::BottomRight),
                margin: Some(pi_ext::protocol::OverlayMarginWire::Uniform(2)),
                ..pi_ext::protocol::OverlaySpec::default()
            }),
        }));

    let buffer = render_view(&state, 40, 12);
    assert_eq!(
        buffer.cell((26, 9)).map(ratatui::buffer::Cell::symbol),
        Some("G")
    );
    assert_ne!(
        buffer.cell((0, 0)).map(ratatui::buffer::Cell::symbol),
        Some("G")
    );
}

#[test]
fn changelog_overlay_snapshot() {
    let mut state = base_state();
    state.overlay = Some(Overlay {
        kind: OverlayKind::Changelog,
        lines: vec![
            "# v0.1.0".to_owned(),
            String::new(),
            "- Initial release".to_owned(),
        ],
        height: 10,
    });
    let buf = render_view(&state, 80, 30);
    let plain = snapshot_buffer_plain(&buf, 80, 30).join("\n");
    assert!(plain.contains("v0.1.0") || plain.contains("Initial release"));
}

#[test]
fn model_selector_renders() {
    use crate::modes::interactive::selectors::build_model_selector;
    use crate::modes::interactive::state::ModelSelectorEntry;
    let entries = vec![
        ModelSelectorEntry {
            value: "anthropic/claude".to_owned(),
            label: "Claude Sonnet".to_owned(),
            description: Some("Fast and capable".to_owned()),
        },
        ModelSelectorEntry {
            value: "openai/gpt".to_owned(),
            label: "GPT-4o".to_owned(),
            description: None,
        },
    ];
    let mut comp = build_model_selector(&entries, 0);
    let buf = render_component(comp.as_mut(), 80);
    let plain = snapshot_buffer_plain(&buf, 80, buf.area().height).join("\n");
    assert!(plain.contains("Claude Sonnet"));
    assert!(plain.contains("GPT-4o"));
    assert!(
        plain.contains("→"),
        "selected row must show the arrow glyph"
    );
}

#[test]
fn session_picker_renders() {
    use crate::modes::interactive::selectors::build_session_picker;
    use crate::modes::interactive::state::SessionPickerEntry;
    let entries = vec![
        SessionPickerEntry {
            value: "s1".to_owned(),
            label: "yesterday — refactor".to_owned(),
            description: None,
        },
        SessionPickerEntry {
            value: "s2".to_owned(),
            label: "today — bugfix".to_owned(),
            description: None,
        },
    ];
    let mut comp = build_session_picker(&entries, 1);
    let buf = render_component(comp.as_mut(), 80);
    let plain = snapshot_buffer_plain(&buf, 80, buf.area().height).join("\n");
    assert!(plain.contains("bugfix"));
    assert!(
        plain.contains("→"),
        "selected (index 1) row shows the arrow"
    );
}

#[test]
fn settings_selector_renders() {
    use crate::modes::interactive::selectors::build_settings_selector;
    use crate::modes::interactive::state::SettingsRow;
    let rows = vec![
        SettingsRow {
            id: "theme".to_owned(),
            label: "Theme".to_owned(),
            description: Some("Color scheme".to_owned()),
            current_value: "dark".to_owned(),
            values: Some(vec!["dark".to_owned(), "light".to_owned()]),
        },
        SettingsRow {
            id: "autoCompact".to_owned(),
            label: "Auto-compact".to_owned(),
            description: None,
            current_value: "on".to_owned(),
            values: Some(vec!["on".to_owned(), "off".to_owned()]),
        },
    ];
    let mut comp = build_settings_selector(&rows, 0);
    let buf = render_component(comp.as_mut(), 80);
    let plain = snapshot_buffer_plain(&buf, 80, buf.area().height).join("\n");
    assert!(plain.contains("Theme"));
    assert!(plain.contains("Auto-compact"));
}

#[test]
fn tree_selector_indents_depth() {
    use crate::modes::interactive::selectors::build_tree_selector;
    use crate::modes::interactive::state::TreeEntry;
    let entries = vec![
        TreeEntry {
            value: "root".to_owned(),
            label: "root".to_owned(),
            depth: 0,
        },
        TreeEntry {
            value: "a".to_owned(),
            label: "branch a".to_owned(),
            depth: 1,
        },
        TreeEntry {
            value: "a1".to_owned(),
            label: "leaf".to_owned(),
            depth: 2,
        },
    ];
    let mut comp = build_tree_selector(&entries, 2);
    let buf = render_component(comp.as_mut(), 80);
    let plain = snapshot_buffer_plain(&buf, 80, buf.area().height).join("\n");
    assert!(
        plain.contains("        leaf") || plain.contains("    leaf"),
        "depth-2 leaf must be indented: {plain}"
    );
    assert!(
        !plain.contains("        root") && !plain.contains("    root"),
        "root must not be depth-indented: {plain}"
    );
}

#[test]
fn scoped_models_selector_marks_enabled() {
    use crate::modes::interactive::selectors::build_scoped_models_selector;
    use crate::modes::interactive::state::ModelSelectorEntry;
    let entries = vec![ModelSelectorEntry {
        value: "anthropic/claude".to_owned(),
        label: "Claude".to_owned(),
        description: None,
    }];
    let mut enabled = std::collections::BTreeMap::new();
    enabled.insert("anthropic/claude".to_owned(), true);
    let mut comp = build_scoped_models_selector(&entries, &enabled, 0);
    let buf = render_component(comp.as_mut(), 80);
    let plain = snapshot_buffer_plain(&buf, 80, buf.area().height).join("\n");
    assert!(plain.contains("[x]"), "enabled model shows [x]: {plain}");
}

#[test]
fn selector_builders_render_named_empty_copy_and_exit_hint() {
    use crate::modes::interactive::selectors::{
        SELECTOR_EXIT_HINT, build_auth_selector, build_config_selector, build_model_selector,
        build_session_picker, build_settings_selector, build_tree_selector,
    };
    use crate::modes::interactive::state::{
        AuthSelectorEntry, ConfigSelectorEntry, ModelSelectorEntry, SessionPickerEntry,
        SettingsRow, TreeEntry,
    };

    let cases: Vec<(&str, Box<dyn pi_tui::component::Component>)> = vec![
        (
            "  No matching models",
            build_model_selector(&[] as &[ModelSelectorEntry], 0),
        ),
        (
            "  No sessions found",
            build_session_picker(&[] as &[SessionPickerEntry], 0),
        ),
        (
            "  No providers available",
            build_auth_selector(&[] as &[AuthSelectorEntry], 0),
        ),
        (
            "  No entries found",
            build_tree_selector(&[] as &[TreeEntry], 0),
        ),
        (
            "  No settings available",
            build_settings_selector(&[] as &[SettingsRow], 0),
        ),
        (
            "  No resources found",
            build_config_selector(&[] as &[ConfigSelectorEntry], 0),
        ),
    ];

    for (expected, mut comp) in cases {
        let buf = render_component(comp.as_mut(), 80);
        let plain = snapshot_buffer_plain(&buf, 80, buf.area().height).join("\n");
        assert!(
            plain.contains(expected.trim()),
            "expected `{expected}` in:\n{plain}"
        );
        assert!(
            plain.contains(SELECTOR_EXIT_HINT.trim()) || plain.contains("Esc to cancel"),
            "exit hint missing for `{expected}`:\n{plain}"
        );
        assert!(
            !plain.contains("No matching commands"),
            "generic fallback leaked for `{expected}`:\n{plain}"
        );
    }
}

// ---------------------------------------------------------------------------
// Resize + theme fallback
// ---------------------------------------------------------------------------

#[test]
fn resize_updates_dimensions_and_renders() {
    let mut state = base_state();
    state.resize(20, 10);
    assert_eq!((state.width, state.height), (20, 10));
    state.resize(160, 50);
    assert_eq!((state.width, state.height), (160, 50));
    for &w in &[10_u16, 20, 40, 80, 120, 160, 200] {
        let _ = render_view(&state, w, 50);
    }
}

#[test]
fn theme_invalid_falls_back_to_dark() {
    let bad = theme::ThemeJson::parse("{ not valid json");
    assert!(bad.is_err());
    let fallback = theme::load_or_dark("nonexistent-theme-xyz", ColorMode::Truecolor);
    assert_eq!(fallback.name, "dark", "missing theme falls back to dark");
}

#[test]
fn custom_theme_round_trips() -> Result<(), String> {
    let json = r##"{
        "name": "test-theme",
        "vars": { "primary": "#ff0000" },
        "colors": {
            "accent": "primary", "border": "#00ff00", "borderAccent": "#0000ff",
            "borderMuted": "#111111", "success": "#222222", "error": "#333333",
            "warning": "#444444", "muted": "#555555", "dim": "#666666",
            "text": "#777777", "thinkingText": "#888888",
            "selectedBg": "#000000", "userMessageBg": "#000000", "userMessageText": "#777777",
            "customMessageBg": "#000000", "customMessageText": "#777777", "customMessageLabel": "#999999",
            "toolPendingBg": "#000000", "toolSuccessBg": "#000000", "toolErrorBg": "#000000",
            "toolTitle": "#777777", "toolOutput": "#555555",
            "mdHeading": "#aaaaaa", "mdLink": "#bbbbbb", "mdLinkUrl": "#cccccc",
            "mdCode": "#dddddd", "mdCodeBlock": "#eeeeee", "mdCodeBlockBorder": "#ffffff",
            "mdQuote": "#111111", "mdQuoteBorder": "#222222", "mdHr": "#333333", "mdListBullet": "#444444",
            "toolDiffAdded": "#55ff55", "toolDiffRemoved": "#ff5555", "toolDiffContext": "#999999",
            "syntaxComment": "#000000", "syntaxKeyword": "#000011", "syntaxFunction": "#000022",
            "syntaxVariable": "#000033", "syntaxString": "#000044", "syntaxNumber": "#000055",
            "syntaxType": "#000066", "syntaxOperator": "#000077", "syntaxPunctuation": "#000088",
            "thinkingOff": "#100000", "thinkingMinimal": "#200000", "thinkingLow": "#300000",
            "thinkingMedium": "#400000", "thinkingHigh": "#500000", "thinkingXhigh": "#600000",
            "bashMode": "#700000"
        }
    }"##;
    let parsed = theme::ThemeJson::parse(json)
        .map_err(|error| format!("valid theme must parse: {error}"))?;
    assert_eq!(parsed.name(), "test-theme");
    let resolved = parsed
        .resolve_owned(ColorMode::Truecolor)
        .map_err(|error| format!("valid theme must resolve: {error}"))?;
    assert_eq!(
        resolved.fg_rgb(ThemeColor::Accent),
        theme::Rgb(255, 0, 0),
        "var ref resolved"
    );
    assert_eq!(
        resolved.fg_rgb(ThemeColor::Border),
        theme::Rgb(0, 255, 0),
        "hex resolved"
    );
    assert_eq!(
        resolved.fg_rgb(ThemeColor::ThinkingMax),
        resolved.fg_rgb(ThemeColor::ThinkingXhigh),
        "thinkingMax falls back to thinkingXhigh"
    );
    Ok(())
}

// ---------------------------------------------------------------------------
// ANSI emission (colors reconstructed from buffer styles)
// ---------------------------------------------------------------------------

#[test]
fn ansi_snapshot_emits_truecolor() {
    let state = base_state();
    let buf = render_view(&state, 80, 30);
    let ansi = snapshot_buffer_ansi(&buf, 80, 30, ColorMode::Truecolor);
    let joined = ansi.join("\n");
    assert!(
        joined.contains("\x1b[38;2;") || joined.contains("\x1b[48;2;"),
        "truecolor SGR expected: {joined}"
    );
}

#[test]
fn ansi_snapshot_emits_palette256() {
    let state = base_state();
    let buf = render_view(&state, 80, 30);
    let ansi = snapshot_buffer_ansi(&buf, 80, 30, ColorMode::Palette256);
    let joined = ansi.join("\n");
    assert!(
        joined.contains("\x1b[38;5;") || joined.contains("\x1b[48;5;"),
        "256-color SGR expected: {joined}"
    );
}

// ---------------------------------------------------------------------------
// Footer + progress unit checks
// ---------------------------------------------------------------------------

#[test]
fn format_tokens_thresholds() {
    assert_eq!(format_tokens(0), "0");
    assert_eq!(format_tokens(999), "999");
    assert_eq!(format_tokens(1500), "1.5k");
    assert_eq!(format_tokens(15_000), "15k");
    assert_eq!(format_tokens(1_500_000), "1.5M");
}

#[test]
fn format_cwd_collapses_home() {
    assert_eq!(
        format_cwd_for_footer("/home/u/projects/pi", "/home/u"),
        "~/projects/pi"
    );
    assert_eq!(format_cwd_for_footer("/home/u", "/home/u"), "~");
    assert_eq!(format_cwd_for_footer("/etc/init", "/home/u"), "/etc/init");
    assert_eq!(format_cwd_for_footer("/anywhere", ""), "/anywhere");
}

#[test]
fn auth_progress_renders_every_stage() {
    use crate::modes::interactive::progress::auth_stage_message;
    let th = theme::dark();
    for stage in [
        OAuthStage::BrowserCallback,
        OAuthStage::DeviceCode,
        OAuthStage::ManualKey,
        OAuthStage::Exchanging,
        OAuthStage::Done,
        OAuthStage::Failed,
    ] {
        let p = AuthProgress {
            stage,
            provider: "anthropic".to_owned(),
            detail: Some("https://example.com/auth".to_owned()),
        };
        let msg = auth_stage_message(&p, &th);
        assert!(!msg.is_empty(), "stage {stage:?} must produce a message");
    }
}

#[test]
fn compaction_and_retry_messages_match_reference() {
    crate::core::keybindings::with_global_app_keybindings(|| {
        use crate::modes::interactive::status::{compaction_message, retry_message};
        assert_eq!(
            compaction_message(CompactionReason::Manual),
            "Compacting context… (escape to cancel)"
        );
        assert_eq!(
            compaction_message(CompactionReason::Overflow),
            "Context overflow detected, auto-compacting… (escape to cancel)"
        );
        assert_eq!(
            retry_message(2, 3, 5),
            "Retrying (2/3) in 5s… (escape to cancel)"
        );
    });
}

// ---------------------------------------------------------------------------
// Theme-scoped rendering (with_theme restores prior)
// ---------------------------------------------------------------------------

#[test]
fn with_theme_restores_prior() {
    let dark = theme::dark();
    let light = theme::light();
    theme::with_theme(dark.clone(), || {
        assert_eq!(theme::current().name, "dark");
        theme::with_theme(light.clone(), || {
            assert_eq!(theme::current().name, "light");
        });
        assert_eq!(
            theme::current().name,
            "dark",
            "inner with_theme must restore"
        );
    });
}

// ---------------------------------------------------------------------------
// Resources / diagnostics / widgets appear in composed output
// ---------------------------------------------------------------------------

#[test]
fn resources_diagnostics_widgets_sections_present() {
    use crate::modes::interactive::state::DiagnosticSeverity;
    let mut state = base_state();
    state.resources.push(LoadedResource {
        kind: "skill".to_owned(),
        label: "commit".to_owned(),
    });
    state.diagnostics.entries.push(StartupDiagnostic {
        severity: DiagnosticSeverity::Warning,
        source: "themes".to_owned(),
        message: "custom theme 'foo' failed to load".to_owned(),
    });
    let widget = |key: &str, text: &str, placement, focusable, focused| WidgetSlot {
        slot: pi_ext::sanitize::sanitize_slot(&pi_ext::protocol::UiSlot {
            key: key.to_owned(),
            generation: 1,
            placement,
            height: 1,
            runs: vec![vec![pi_ext::protocol::StyledRun {
                text: text.to_owned(),
                style: pi_ext::protocol::Style::default(),
            }]],
            focusable,
            cursor: None,
            overlay_options: None,
        }),
        focused,
    };
    state.widgets_above.push(widget(
        "ext1",
        "[ext] widget above editor",
        pi_ext::protocol::SlotPlacement::AboveEditor,
        false,
        false,
    ));
    state.widgets_below.push(widget(
        "ext2",
        "[ext] widget below editor",
        pi_ext::protocol::SlotPlacement::BelowEditor,
        true,
        false,
    ));
    let buf = render_view(&state, 80, 40);
    let plain = snapshot_buffer_plain(&buf, 80, 40).join("\n");
    assert!(plain.contains("skill"), "resource must render");
    assert!(
        plain.contains("themes") || plain.contains("custom theme"),
        "diagnostic must render: {plain}"
    );
    assert!(plain.contains("widget above editor"));
    assert!(plain.contains("widget below editor"));
}

#[test]
fn default_shortcut_hints_are_nonempty() {
    use crate::modes::interactive::startup::default_shortcut_hints;
    crate::core::keybindings::with_global_app_keybindings(|| {
        let hints: Vec<ShortcutHint> = default_shortcut_hints();
        assert!(!hints.is_empty());
        assert!(hints.iter().any(|h| h.key == "Escape"));
        // The overlay row is the raw slash command — not the legacy
        // `? , /hotkeys` pair with its stray space.
        assert!(
            hints
                .iter()
                .any(|h| h.key == "/hotkeys" && h.action == "This overlay")
        );
        for hint in &hints {
            assert!(
                !hint.key.contains('?') && !hint.key.contains(", "),
                "key column must be one clean key: {}",
                hint.key
            );
        }
    });
}

#[test]
fn rendered_key_hints_follow_rebinds() {
    use crate::modes::interactive::messages::ToolMessageView;
    use crate::modes::interactive::tool_renderer::{
        ToolCallView, ToolPhase, ToolResultView, ToolState,
    };

    crate::core::keybindings::with_global_app_keybindings(|| {
        // Rebind two hinted actions; every rendered site must follow.
        let mut user = pi_tui::keybindings::KeybindingsConfig::new();
        user.insert(
            "app.tools.expand".to_owned(),
            vec![pi_tui::keys::KeyId::from_raw("ctrl+m")],
        );
        user.insert(
            "app.thinking.cycle".to_owned(),
            vec![pi_tui::keys::KeyId::from_raw("f9")],
        );
        pi_tui::keybindings::set_keybindings(pi_tui::keybindings::KeybindingsManager::new(
            crate::core::keybindings::app_keybindings(),
            user,
        ));

        // Empty-state header hint renders the rebound keys.
        let mut state = base_state();
        let buf = render_view(&state, 80, 30);
        let plain = snapshot_buffer_plain(&buf, 80, 30).join("\n");
        assert!(
            plain.contains("/hotkeys shortcuts · ctrl+m expand tools · f9 thinking"),
            "empty-state hint must render rebound keys: {plain}"
        );

        // Tool-collapse hint (15 result lines → 12 preview + 3 hidden).
        state.messages.push(StateMessageView::Tool(ToolMessageView {
            renderer: "read".to_owned(),
            state: ToolState {
                call: ToolCallView {
                    name: "read".to_owned(),
                    id: "call_1".to_owned(),
                    args_summary: "path: src/lib.rs".to_owned(),
                    raw_args: serde_json::json!({ "path": "src/lib.rs" }),
                },
                result: Some(ToolResultView {
                    text: (1..=15)
                        .map(|i| format!("line {i}"))
                        .collect::<Vec<_>>()
                        .join("\n"),
                    truncated: false,
                    full_output_path: None,
                    images: Vec::new(),
                    error: None,
                }),
                expanded: false,
                phase: ToolPhase::Success,
            },
        }));
        let buf = render_view(&state, 80, 40);
        let plain = snapshot_buffer_plain(&buf, 80, 40).join("\n");
        assert!(
            plain.contains("… 3 more lines · ctrl+m"),
            "tool collapse hint must render the rebound expand key: {plain}"
        );

        // Status cancel hint still resolves app.interrupt from the registry.
        state.status = Some(SessionStatus {
            kind: StatusKind::Working,
            frame: 0,
            elapsed_secs: 0,
            message: "Working…".to_owned(),
        });
        let buf = render_view(&state, 80, 40);
        let plain = snapshot_buffer_plain(&buf, 80, 40).join("\n");
        assert!(
            plain.contains("· escape to cancel"),
            "status cancel hint must resolve app.interrupt: {plain}"
        );

        // Shortcut overlay renders the rebound key capitalized.
        state.overlay = Some(Overlay {
            kind: OverlayKind::ShortcutHelp,
            lines: Vec::new(),
            height: 20,
        });
        let buf = render_view(&state, 80, 40);
        let plain = snapshot_buffer_plain(&buf, 80, 40).join("\n");
        assert!(
            plain.contains("Ctrl+M"),
            "overlay key column must render the capitalized rebind: {plain}"
        );
        // Expanded header hints follow rebinds too.
        state.header.expanded = true;
        let buf = render_view(&state, 80, 60);
        let plain = snapshot_buffer_plain(&buf, 80, 60).join("\n");
        assert!(
            plain.contains("Ctrl+M") && plain.contains("F9"),
            "expanded header must render rebound keys: {plain}"
        );
    });
}

#[test]
fn bash_progress_builder_renders_command() {
    use crate::modes::interactive::progress::build_bash_progress;
    let p = BashProgress {
        command: "ls -la".to_owned(),
        output: "total 0".to_owned(),
        expanded: false,
        exit_code: None,
        cancelled: false,
    };
    let th = theme::dark();
    let mut comp = build_bash_progress(&p, &th);
    let buf = render_component(comp.as_mut(), 80);
    let plain = snapshot_buffer_plain(&buf, 80, buf.area().height).join("\n");
    assert!(
        plain.contains("ls -la"),
        "bash command must render: {plain}"
    );
}

#[test]
fn empty_diagnostics_render_zero_height() -> Result<(), String> {
    let state = base_state();
    let view = compose(&state);
    let mut diag = view
        .sections
        .into_iter()
        .find(|section| section.label == "diagnostics")
        .ok_or_else(|| "composed view must include diagnostics section".to_owned())?;
    let h = diag.component.measure(80);
    assert_eq!(h, 0, "empty diagnostics must have zero height");
    Ok(())
}

#[test]
fn compaction_retry_auth_progress_render() {
    use crate::modes::interactive::progress::{
        build_auth_progress, build_compaction_progress, build_retry_progress,
    };
    let th = theme::dark();
    let compaction = CompactionProgress {
        reason: CompactionReason::Manual,
    };
    let mut c = build_compaction_progress(&compaction, &th);
    let cbuf = render_component(c.as_mut(), 80);
    let cplain = snapshot_buffer_plain(&cbuf, 80, cbuf.area().height).join("\n");
    assert!(
        cplain.contains("Compacting context"),
        "compaction progress: {cplain}"
    );

    let retry = RetryProgress {
        attempt: 1,
        max_attempts: 3,
        seconds: 4,
    };
    let mut r = build_retry_progress(&retry, &th);
    let rbuf = render_component(r.as_mut(), 80);
    let rplain = snapshot_buffer_plain(&rbuf, 80, rbuf.area().height).join("\n");
    assert!(
        rplain.contains("Retrying (1/3)"),
        "retry progress: {rplain}"
    );

    let auth = AuthProgress {
        stage: OAuthStage::BrowserCallback,
        provider: "anthropic".to_owned(),
        detail: Some("https://example.com/auth".to_owned()),
    };
    let mut a = build_auth_progress(&auth, &th);
    let abuf = render_component(a.as_mut(), 80);
    let aplain = snapshot_buffer_plain(&abuf, 80, abuf.area().height).join("\n");
    assert!(
        aplain.contains("Opening browser"),
        "auth progress: {aplain}"
    );
}
