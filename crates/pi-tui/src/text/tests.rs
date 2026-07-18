//! Ports of TS width / wrap / truncate / slice / tab / RI regression tables.

use proptest::prelude::*;
use unicode_segmentation::UnicodeSegmentation;

use super::{
    CURSOR_MARKER, composite_line_at, extract_ansi_code, extract_segments, find_cursor_marker,
    grapheme_width, is_partial_closing_fence_line, normalize_terminal_output, slice_by_column,
    slice_with_width, strip_cursor_marker, strip_trailing_partial_closing_fence, truncate_to_width,
    visible_width, wrap_text_with_ansi,
};

#[test]
fn printable_ascii_width() {
    assert_eq!(visible_width(""), 0);
    assert_eq!(visible_width("hello"), 5);
    assert_eq!(visible_width("a\tb"), 5);
}

#[test]
fn tab_counts_as_three() {
    assert_eq!(grapheme_width("\t"), 3);
    assert_eq!(visible_width("\t"), 3);
    assert_eq!(visible_width("\t\u{1b}[31m界\u{1b}[0m"), 5);
}

#[test]
fn regional_indicator_singleton_and_pair() -> Result<(), String> {
    assert_eq!(visible_width("🇨"), 2);
    assert_eq!(visible_width("🇨🇳"), 2);
    assert_eq!(visible_width("      - 🇨"), 10);
    for cp in 0x1f1e6u32..=0x1f1ff {
        let scalar = char::from_u32(cp).ok_or_else(|| format!("invalid scalar U+{cp:X}"))?;
        let regional_indicator = scalar.to_string();
        assert_eq!(visible_width(&regional_indicator), 2, "U+{cp:X}");
    }
    for flag in ["🇯🇵", "🇺🇸", "🇬🇧", "🇨🇳", "🇩🇪", "🇫🇷"] {
        assert_eq!(visible_width(flag), 2, "{flag}");
    }
    Ok(())
}

#[test]
fn streaming_emoji_intermediates() {
    for sample in ["👍", "👍🏻", "✅", "⚡", "⚡️", "👨", "👨‍💻", "🏳️‍🌈"]
    {
        assert_eq!(visible_width(sample), 2, "{sample}");
    }
}

#[test]
fn thai_lao_am_widths_and_normalize() {
    assert_eq!(visible_width("ำ"), 1);
    assert_eq!(visible_width("ຳ"), 1);
    assert_eq!(visible_width("กำ"), 2);
    assert_eq!(visible_width("ກຳ"), 2);
    assert_eq!(normalize_terminal_output("ำ"), "ํา");
    assert_eq!(normalize_terminal_output("ຳ"), "ໍາ");
    assert_eq!(
        visible_width(&normalize_terminal_output("ำabc")),
        visible_width("ำabc")
    );
}

#[test]
fn osc_and_apc_ignored_in_width() {
    assert_eq!(visible_width("\u{1b}]133;A\u{7}hello\u{1b}]133;B\u{7}"), 5);
    assert_eq!(
        visible_width("\u{1b}]133;A\u{1b}\\hello\u{1b}]133;B\u{1b}\\"),
        5
    );
    assert_eq!(visible_width(&format!("hi{CURSOR_MARKER}")), 2);
}

#[test]
fn normalize_leaves_tabs_inside_escapes() {
    let control_sequences = [
        "\u{1b}]8;;https://example.test/a\tb\u{7}",
        "\u{1b}]0;window\ttitle\u{1b}\\",
        "\u{1b}_payload\tdata\u{1b}\\",
    ];
    for control in control_sequences {
        assert_eq!(
            normalize_terminal_output(&format!("{control}label\ttext")),
            format!("{control}label   text")
        );
    }
}

#[test]
fn wrap_basic_and_line_endings() {
    assert_eq!(
        wrap_text_with_ansi("first\nsecond\r\nthird\rfourth", 80),
        vec!["first", "second", "third", "fourth"]
    );
    let red = "\u{1b}[31m";
    let reset = "\u{1b}[0m";
    assert_eq!(
        wrap_text_with_ansi(&format!("{red}first\r\nsecond\rthird{reset}"), 80),
        vec![
            format!("{red}first"),
            format!("{red}second"),
            format!("{red}third{reset}"),
        ]
    );
}

#[test]
fn wrap_cjk_and_color() {
    let text = "This is an example 中文汉字测试段落内容中文汉字测试段落内容.";
    let wrapped = wrap_text_with_ansi(text, 40);
    assert_eq!(
        wrapped,
        vec![
            "This is an example 中文汉字测试段落内容".to_owned(),
            "中文汉字测试段落内容.".to_owned()
        ]
    );
    for line in &wrapped {
        assert!(visible_width(line) <= 40);
    }

    let red = "\u{1b}[31m";
    let reset = "\u{1b}[0m";
    let colored = format!("{red}{text}{reset}");
    let wrapped = wrap_text_with_ansi(&colored, 40);
    assert_eq!(wrapped.len(), 2);
    assert_eq!(
        wrapped[0],
        format!("{red}This is an example 中文汉字测试段落内容")
    );
    assert_eq!(wrapped[1], format!("{red}中文汉字测试段落内容.{reset}"));
}

#[test]
fn wrap_partial_flag_list_line() {
    let wrapped = wrap_text_with_ansi("      - 🇨", 9);
    assert_eq!(wrapped.len(), 2);
    assert_eq!(visible_width(&wrapped[0]), 7);
    assert_eq!(visible_width(&wrapped[1]), 2);
}

#[test]
fn wrap_underline_and_background() {
    let underline_on = "\u{1b}[4m";
    let underline_off = "\u{1b}[24m";
    let url = "https://example.com/very/long/path/that/will/wrap";
    let text = format!("read this thread {underline_on}{url}{underline_off}");
    let wrapped = wrap_text_with_ansi(&text, 40);
    assert_eq!(wrapped[0], "read this thread");
    assert!(wrapped[1].starts_with(underline_on));
    assert!(wrapped[1].contains("https://"));

    let bg_blue = "\u{1b}[44m";
    let reset = "\u{1b}[0m";
    let text = format!("{bg_blue}hello world this is blue background text{reset}");
    let wrapped = wrap_text_with_ansi(&text, 15);
    for line in &wrapped {
        assert!(line.contains(bg_blue));
    }
    for line in wrapped.iter().take(wrapped.len().saturating_sub(1)) {
        assert!(!line.ends_with("\u{1b}[0m"));
    }
}

#[test]
fn wrap_osc8_hyperlinks_st_and_bel() {
    let url = "https://example.com";
    let input = format!("\u{1b}]8;;{url}\u{1b}\\0123456789\u{1b}]8;;\u{1b}\\");
    let lines = wrap_text_with_ansi(&input, 6);
    assert!(lines.len() > 1);
    for line in &lines {
        let stripped = strip_escapes_for_check(line);
        if !stripped.trim().is_empty() {
            assert!(
                line.contains(&format!("\u{1b}]8;;{url}\u{1b}\\")),
                "missing OSC8 reopen: {line:?}"
            );
        }
    }
    for line in lines.iter().take(lines.len() - 1) {
        if line.contains(&format!("\u{1b}]8;;{url}\u{1b}\\")) {
            assert!(
                line.ends_with("\u{1b}]8;;\u{1b}\\"),
                "missing OSC8 close: {line:?}"
            );
        }
    }

    let oauth = format!("https://example.com/oauth/{}", "a".repeat(32));
    let input = format!("\u{1b}]8;;{oauth}\u{7}{oauth}\u{1b}]8;;\u{7}");
    let lines = wrap_text_with_ansi(&input, 20);
    assert!(lines.len() > 1);
    for line in &lines {
        assert!(line.contains(&format!("\u{1b}]8;;{oauth}\u{7}")));
        assert!(!line.contains(&format!("\u{1b}]8;;{oauth}\u{1b}\\")));
    }
    for line in lines.iter().take(lines.len() - 1) {
        assert!(line.ends_with("\u{1b}]8;;\u{7}"));
    }
}

fn strip_escapes_for_check(line: &str) -> String {
    let mut out = String::new();
    let mut i = 0;
    while i < line.len() {
        if let Some(ansi) = extract_ansi_code(line, i) {
            i += ansi.len;
        } else {
            let Some(ch) = line[i..].chars().next() else {
                break;
            };
            out.push(ch);
            i += ch.len_utf8();
        }
    }
    out
}

#[test]
fn truncate_tables() {
    let text = "🙂界".repeat(1000);
    let truncated = truncate_to_width(&text, 40, "…", false);
    assert!(visible_width(&truncated) <= 40);
    assert!(truncated.ends_with("…\u{1b}[0m"));

    let text = format!("\u{1b}[31m{}{}", "hello ".repeat(1000), "\u{1b}[0m");
    let truncated = truncate_to_width(&text, 20, "…", false);
    assert!(visible_width(&truncated) <= 20);
    assert!(truncated.contains("\u{1b}[31m"));
    assert!(truncated.ends_with("\u{1b}[0m…\u{1b}[0m"));

    let text = format!("abc\u{1b}not-ansi {}", "🙂".repeat(100));
    let truncated = truncate_to_width(&text, 20, "…", false);
    assert!(visible_width(&truncated) <= 20);

    assert_eq!(truncate_to_width("abcdef", 1, "🙂", false), "");
    assert_eq!(
        truncate_to_width("abcdef", 2, "🙂", false),
        "\u{1b}[0m🙂\u{1b}[0m"
    );
    assert_eq!(truncate_to_width("a", 2, "🙂", false), "a");
    assert_eq!(truncate_to_width("界", 2, "🙂", false), "界");

    let truncated = truncate_to_width("🙂界🙂界🙂界", 8, "…", true);
    assert_eq!(visible_width(&truncated), 8);

    let truncated = truncate_to_width(&format!("\u{1b}[31m{}", "hello".repeat(100)), 10, "", false);
    assert!(visible_width(&truncated) <= 10);
    assert!(truncated.ends_with("\u{1b}[0m"));

    let truncated = truncate_to_width("🙂\t界 \u{1b}_abc\u{7}", 7, "…", true);
    assert_eq!(truncated, "🙂\t\u{1b}[0m…\u{1b}[0m ");
}

#[test]
fn slice_and_segments_tabs() {
    let text = "out 192M\t.pi/skill-tests/results-ha";
    let slice = slice_with_width(text, 0, 10, true);
    assert_eq!(slice.text, "out 192M");
    assert_eq!(slice.width, 8);
    assert_eq!(visible_width(&slice.text), slice.width);

    let segments = extract_segments(text, 10, 13, 10, true);
    assert_eq!(segments.before, "out 192M");
    assert_eq!(segments.before_width, 8);
    assert_eq!(visible_width(&segments.before), segments.before_width);

    let tab_fits = extract_segments(text, 11, 13, 10, true);
    assert_eq!(tab_fits.before, "out 192M\t");
    assert_eq!(tab_fits.before_width, 11);
    assert_eq!(visible_width(&tab_fits.before), tab_fits.before_width);

    assert_eq!(slice_by_column("hello world", 6, 5, false), "world");
}

#[test]
fn composite_line_basic() {
    let base = "abcdefghij";
    let overlay = "XY";
    let out = composite_line_at(base, overlay, 3, 2, 10);
    // before "abc" + pad0 + reset + "XY" + pad0 + reset + after + pad
    assert!(out.contains("abc"));
    assert!(out.contains("XY"));
    assert!(visible_width(&out) <= 10);
}

#[test]
fn cursor_marker_helpers() -> Result<(), String> {
    let line = format!("hi{CURSOR_MARKER}there");
    let (idx, col) =
        find_cursor_marker(&line).ok_or_else(|| "cursor marker not found".to_owned())?;
    assert_eq!(col, 2);
    assert_eq!(idx, 2);
    assert_eq!(strip_cursor_marker(&line), "hithere");
    Ok(())
}

#[test]
fn partial_fence_streaming() {
    assert!(is_partial_closing_fence_line("``", "```"));
    assert!(!is_partial_closing_fence_line("```", "```"));
    let raw = "```ts\nconst x = 1;\n``";
    let stripped = strip_trailing_partial_closing_fence(raw);
    assert_eq!(stripped, "```ts\nconst x = 1;");
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(64))]

    #[test]
    fn visible_width_never_panics(s in "\\PC{0,80}") {
        let _ = visible_width(&s);
    }

    #[test]
    fn wrap_lines_respect_width(s in "\\PC{0,60}", w in 1usize..40) {
        for line in wrap_text_with_ansi(&s, w) {
            let mut plain = String::new();
            let mut i = 0;
            while i < line.len() {
                if let Some(ansi) = extract_ansi_code(&line, i) {
                    i += ansi.len;
                } else {
                    let Some(ch) = line[i..].chars().next() else {
                        break;
                    };
                    plain.push(ch);
                    i += ch.len_utf8();
                }
            }
            // A single unbreakable grapheme wider than `w` may still occupy a line
            // (matches TS breakLongWord: it never splits a grapheme).
            let max_g = plain
                .graphemes(true)
                .map(crate::text::grapheme_width)
                .max()
                .unwrap_or(0);
            prop_assert!(visible_width(&plain) <= w.max(max_g));
        }
    }

    #[test]
    fn truncate_respects_width(s in "\\PC{0,80}", w in 1usize..40) {
        let t = truncate_to_width(&s, w, "...", false);
        prop_assert!(visible_width(&t) <= w);
    }

    #[test]
    fn slice_width_consistent(
        s in "[ -~\\t\\u{4e00}-\\u{4e10}]{0,40}",
        start in 0usize..20,
        len in 0usize..20
    ) {
        let slice = slice_with_width(&s, start, len, true);
        prop_assert_eq!(visible_width(&slice.text), slice.width);
        prop_assert!(slice.width <= len);
    }
}
