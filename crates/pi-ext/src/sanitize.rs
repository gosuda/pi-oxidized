//! Authoritative sanitizer for inbound host UI-slot content.
//!
//! Rust is the validation boundary for anything a TypeScript extension emits
//! through a [`crate::protocol::UiSlot`]. The host is *supposed* to send
//! structured [`crate::protocol::StyledRun`] values with typed
//! [`crate::protocol::Style`] / [`crate::protocol::WireColor`] /
//! [`crate::protocol::Hyperlink`] fields, but a hostile or buggy extension may
//! embed raw escape sequences in the free-form `text` field or split a control
//! sequence across run or generation boundaries. This module strips all of it
//! and returns a plain, safe [`SanitizedSlot`] that the native renderer can
//! paint directly.
//!
//! # Invariants
//!
//! - Every plugin push is parsed from a **fresh ground-state** parser; an
//!   incomplete escape sequence never carries into the next push or generation.
//! - Within one slot, all runs share a single parser so a control sequence
//!   split across runs is still detected and stripped.
//! - Only printable graphemes survive; tabs expand to spaces. Every CSI, OSC,
//!   DCS, APC, C0 (except tab), C1, and embedded newline is dropped.
//! - Structured style/color fields are validated; invalid hyperlinks are
//!   dropped (plain fallback) rather than failing the whole slot.
//! - Dimensions, run counts, and byte totals are clamped to fixed caps.

use anstyle_parse::{Parser, Perform};

use crate::protocol::{
    Hyperlink, OverlaySpec, SlotCursor, SlotPlacement, Style, StyledRun, UiSlot,
};

/// Maximum number of display lines accepted in one slot.
pub const MAX_SLOT_LINES: usize = 4096;
/// Maximum number of styled runs accepted on a single line.
pub const MAX_RUNS_PER_LINE: usize = 1024;
/// Maximum total UTF-8 bytes of sanitized run text across one slot.
pub const MAX_SLOT_TEXT_BYTES: usize = 1 << 20; // 1 MiB
/// Maximum UTF-8 bytes for a single run's sanitized text.
pub const MAX_RUN_TEXT_BYTES: usize = 64 * 1024;
/// Number of spaces a horizontal tab expands to.
pub const TAB_WIDTH: usize = 4;
/// Maximum accepted [`Hyperlink`] id length in bytes.
pub const MAX_LINK_ID_BYTES: usize = Hyperlink::MAX_ID_BYTES;
/// Maximum accepted [`Hyperlink`] uri length in bytes.
pub const MAX_LINK_URI_BYTES: usize = Hyperlink::MAX_URI_BYTES;

/// A single sanitized styled run (text + validated style).
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct SanitizedRun {
    /// Printable text only (tabs expanded, no escapes or newlines).
    pub text: String,
    /// Validated style; invalid sub-fields (e.g. links) dropped.
    pub style: Style,
}

/// A fully validated, render-ready UI slot.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct SanitizedSlot {
    /// Stable slot key.
    pub key: String,
    /// Monotonic generation.
    pub generation: u64,
    /// Composition placement.
    pub placement: SlotPlacement,
    /// Authoritative measured height (`lines.len()` clamped to `u16`).
    pub height: u16,
    /// Validated lines of sanitized runs.
    pub lines: Vec<Vec<SanitizedRun>>,
    /// Whether the slot can receive focus / input.
    pub focusable: bool,
    /// Optional hardware-cursor hint.
    pub cursor: Option<SlotCursor>,
    /// Overlay layout options.
    pub overlay_options: Option<OverlaySpec>,
    /// `true` when at least one escape/control/oversize fragment was stripped
    /// or clamped. The client may surface this as an `extension_error`.
    pub had_rejections: bool,
}

/// `Perform` sink that keeps printable chars, expands tabs, and flags escapes.
struct StripPerformer {
    out: String,
    run_bytes: usize,
    run_byte_cap: usize,
    saw_control: bool,
}

impl StripPerformer {
    fn new(run_byte_cap: usize) -> Self {
        Self {
            out: String::new(),
            run_bytes: 0,
            run_byte_cap,
            saw_control: false,
        }
    }

    fn push_char(&mut self, c: char) {
        if self.run_bytes >= self.run_byte_cap {
            self.saw_control = true;
            return;
        }
        // Respect the per-run cap on a byte granularity.
        let need = c.len_utf8();
        if self.run_bytes.saturating_add(need) > self.run_byte_cap {
            self.saw_control = true;
            return;
        }
        self.out.push(c);
        self.run_bytes = self.run_bytes.saturating_add(need);
    }
}

impl Perform for StripPerformer {
    fn print(&mut self, c: char) {
        self.push_char(c);
    }

    fn execute(&mut self, byte: u8) {
        // C0/C1 controls. Tab expands to spaces; everything else is dropped.
        if byte == b'\t' {
            for _ in 0..TAB_WIDTH {
                self.push_char(' ');
            }
            return;
        }
        // Newlines, carriage returns, and all other controls never survive.
        self.saw_control = true;
    }

    fn hook(
        &mut self,
        _params: &anstyle_parse::Params,
        _intermediates: &[u8],
        _ignore: bool,
        _action: u8,
    ) {
        // DCS entry: strip.
        self.saw_control = true;
    }

    fn put(&mut self, _byte: u8) {
        // DCS body: strip.
        self.saw_control = true;
    }

    fn unhook(&mut self) {
        // DCS end: strip.
        self.saw_control = true;
    }

    fn osc_dispatch(&mut self, _params: &[&[u8]], _bell_terminated: bool) {
        // Raw OSC (clipboard/title/image/synchronized-output): strip. Structured
        // OSC 8 hyperlinks arrive via the typed Style.link field, not here.
        self.saw_control = true;
    }

    fn csi_dispatch(
        &mut self,
        _params: &anstyle_parse::Params,
        _intermediates: &[u8],
        _ignore: bool,
        _action: u8,
    ) {
        // Any CSI (SGR, cursor, erase, clear, DEC private, synchronized-output):
        // strip. Structured styles arrive via the typed Style field.
        self.saw_control = true;
    }
}

/// Strip all escapes/controls from a single text fragment.
///
/// Returns the cleaned text and whether any control byte was seen. The parser
/// is consumed (ground state for the next caller).
#[must_use]
pub fn sanitize_text(text: &str) -> (String, bool) {
    let mut performer = StripPerformer::new(MAX_RUN_TEXT_BYTES);
    // Any ESC byte flags hostile input even when silently consumed (APC/PM/SOS).
    if text.as_bytes().contains(&0x1b) {
        performer.saw_control = true;
    }
    let mut parser = anstyle_parse::Parser::<anstyle_parse::Utf8Parser>::default();
    for &b in text.as_bytes() {
        parser.advance(&mut performer, b);
    }
    (performer.out, performer.saw_control)
}

/// Validate a typed [`Style`], returning a clean copy with invalid sub-fields
/// dropped. Colors are already constrained by the wire enum; the only free-form
/// field is the optional hyperlink.
fn sanitize_style(style: &Style) -> (Style, bool) {
    let mut out = style.clone();
    let mut rejected = false;
    if let Some(link) = &out.link
        && link.validate().is_err()
    {
        out.link = None;
        rejected = true;
    }
    (out, rejected)
}

/// Sanitize one inbound [`UiSlot`] into a render-ready [`SanitizedSlot`].
///
/// Never panics and never returns raw extension bytes: control fragments split
/// across runs or generations are stripped, the parser resets to ground state
/// per slot, and every dimension/byte cap is enforced. Oversize or hostile
/// input degrades to a plain safe fallback rather than failing.
#[must_use]
pub fn sanitize_slot(slot: &UiSlot) -> SanitizedSlot {
    let mut had_rejections = false;
    let mut total_bytes = 0usize;

    // One parser per slot push: catches cross-run escape fragments while
    // guaranteeing a fresh ground state for the next push/generation.
    let mut parser = anstyle_parse::Parser::<anstyle_parse::Utf8Parser>::default();

    let line_count = slot.runs.len().min(MAX_SLOT_LINES);
    if slot.runs.len() > MAX_SLOT_LINES {
        had_rejections = true;
    }

    let mut lines: Vec<Vec<SanitizedRun>> = Vec::with_capacity(line_count);
    for line in slot.runs.iter().take(MAX_SLOT_LINES) {
        let run_count = line.len().min(MAX_RUNS_PER_LINE);
        if line.len() > MAX_RUNS_PER_LINE {
            had_rejections = true;
        }
        let mut out_line: Vec<SanitizedRun> = Vec::with_capacity(run_count);
        for run in line.iter().take(MAX_RUNS_PER_LINE) {
            let sanitized = sanitize_run(run, &mut parser, &mut total_bytes, &mut had_rejections);
            out_line.push(sanitized);
        }
        lines.push(out_line);
    }

    let height = u16::try_from(lines.len()).unwrap_or(u16::MAX);

    SanitizedSlot {
        key: slot.key.clone(),
        generation: slot.generation,
        placement: slot.placement,
        height,
        lines,
        focusable: slot.focusable,
        cursor: slot.cursor,
        overlay_options: slot.overlay_options.clone(),
        had_rejections,
    }
}

fn sanitize_run(
    run: &StyledRun,
    parser: &mut Parser,
    total_bytes: &mut usize,
    had_rejections: &mut bool,
) -> SanitizedRun {
    let mut performer = StripPerformer::new(MAX_RUN_TEXT_BYTES);
    // Tighten the per-run cap by the remaining slot budget so the slot cap holds.
    let remaining_slot = MAX_SLOT_TEXT_BYTES.saturating_sub(*total_bytes);
    performer.run_byte_cap = performer.run_byte_cap.min(remaining_slot);
    // Reliably flag hostile input: any ESC byte means an escape sequence is
    // present, even if the parser silently consumes it (APC / PM / SOS strings
    // and ESC-dispatch have no Perform callback). The bytes are still stripped.
    if run.text.as_bytes().contains(&0x1b) {
        performer.saw_control = true;
    }

    for &b in run.text.as_bytes() {
        parser.advance(&mut performer, b);
    }
    if performer.saw_control {
        *had_rejections = true;
    }
    *total_bytes = total_bytes.saturating_add(performer.out.len());
    if !run.text.is_empty() && performer.out.is_empty() && performer.run_bytes == 0 {
        // All bytes were control data.
        *had_rejections = true;
    }

    let (style, style_rejected) = sanitize_style(&run.style);
    if style_rejected {
        *had_rejections = true;
    }

    SanitizedRun {
        text: performer.out,
        style,
    }
}

/// Returns `true` when `bytes` contains any byte that is not a printable
/// grapheme, tab, or newline. Useful for assertions that plugin text never
/// reached a raw output channel.
#[must_use]
pub fn contains_control_bytes(bytes: &[u8]) -> bool {
    let mut saw_control = false;
    let mut detector = ControlDetector {
        saw: &mut saw_control,
    };
    let mut parser = anstyle_parse::Parser::<anstyle_parse::Utf8Parser>::default();
    for &b in bytes {
        parser.advance(&mut detector, b);
    }
    saw_control
}

struct ControlDetector<'a> {
    saw: &'a mut bool,
}

impl Perform for ControlDetector<'_> {
    fn print(&mut self, _c: char) {}
    fn execute(&mut self, _byte: u8) {
        *self.saw = true;
    }
    fn hook(&mut self, _: &anstyle_parse::Params, _: &[u8], _: bool, _: u8) {
        *self.saw = true;
    }
    fn put(&mut self, _: u8) {
        *self.saw = true;
    }
    fn unhook(&mut self) {
        *self.saw = true;
    }
    fn osc_dispatch(&mut self, _: &[&[u8]], _: bool) {
        *self.saw = true;
    }
    fn csi_dispatch(&mut self, _: &anstyle_parse::Params, _: &[u8], _: bool, _: u8) {
        *self.saw = true;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::protocol::{NamedColor, WireColor};

    fn run(text: &str) -> StyledRun {
        StyledRun {
            text: text.to_owned(),
            style: Style::default(),
        }
    }

    fn slot(lines: Vec<Vec<StyledRun>>) -> UiSlot {
        UiSlot {
            key: "k".to_owned(),
            generation: 1,
            placement: SlotPlacement::AboveEditor,
            height: u16::try_from(lines.len()).unwrap_or(u16::MAX),
            runs: lines,
            focusable: false,
            cursor: None,
            overlay_options: None,
        }
    }

    #[test]
    fn strips_sgr_and_keeps_printable() {
        let (text, ctrl) = sanitize_text("hi \u{1b}[31mred\u{1b}[0m end");
        assert_eq!(text, "hi red end");
        assert!(ctrl);
    }

    #[test]
    fn expands_tabs_and_drops_newlines() {
        let (text, ctrl) = sanitize_text("a\tb\nc");
        assert_eq!(text, "a    bc");
        assert!(ctrl);
    }

    #[test]
    fn strips_csi_cursor_osc_dcs() {
        let cases = [
            "\u{1b}[2Jclear",
            "\u{1b}[Hmove",
            "\u{1b}]0;title\u{7}x",
            "\u{1b}P1$q\u{1b}\\dcs",
            "\u{1b}[?2026hsync",
            "\u{1b}_G;k=i;APC\u{1b}\\x",
        ];
        for input in cases {
            let (text, ctrl) = sanitize_text(input);
            assert!(!contains_control_bytes(text.as_bytes()), "leak: {input:?}");
            assert!(ctrl, "expected control flag for {input:?}");
        }
    }

    #[test]
    fn splits_escape_across_runs_within_slot() {
        let s = sanitize_slot(&slot(vec![vec![run("ab\u{1b}[")], vec![run("31m cd")]]));
        assert_eq!(s.lines.len(), 2);
        assert_eq!(s.lines[0][0].text, "ab");
        assert_eq!(s.lines[1][0].text, " cd");
        assert!(s.had_rejections);
        for line in &s.lines {
            for r in line {
                assert!(!contains_control_bytes(r.text.as_bytes()));
            }
        }
    }

    #[test]
    fn resets_parser_ground_state_across_slots() {
        // Slot 1 ends mid-escape; slot 2 must start from a fresh ground state.
        let s1 = sanitize_slot(&slot(vec![vec![run("x\u{1b}[")]]));
        let s2 = sanitize_slot(&slot(vec![vec![run("31m normal")]]));
        // The incomplete CSI from slot 1 is stripped; nothing leaks.
        assert_eq!(s1.lines[0][0].text, "x");
        assert!(s1.had_rejections);
        assert!(!contains_control_bytes(s1.lines[0][0].text.as_bytes()));
        // A fresh parser treats the dangling "31m" continuation as plain text:
        // without a preceding ESC [ it is not a CSI, so it survives literally.
        // The key invariant — incomplete escape state never carries across
        // pushes — holds because the bytes are independent and ESC-free.
        assert_eq!(s2.lines[0][0].text, "31m normal");
        assert!(!s2.had_rejections);
        assert!(!contains_control_bytes(s2.lines[0][0].text.as_bytes()));
    }

    #[test]
    fn drops_invalid_hyperlink_keeps_rest() {
        let mut styled = run("link");
        styled.style.link = Some(Hyperlink {
            id: None,
            uri: "javascript:alert(1)".to_owned(),
        });
        let s = sanitize_slot(&slot(vec![vec![styled]]));
        assert_eq!(s.lines[0][0].text, "link");
        assert!(s.lines[0][0].style.link.is_none());
        assert!(s.had_rejections);
    }

    #[test]
    fn keeps_valid_hyperlink_and_colors() {
        let mut styled = run("go");
        styled.style.bold = Some(true);
        styled.style.fg = Some(WireColor::Named {
            name: NamedColor::Green,
        });
        styled.style.link = Some(Hyperlink {
            id: None,
            uri: "https://example.com".to_owned(),
        });
        let s = sanitize_slot(&slot(vec![vec![styled]]));
        assert_eq!(s.lines[0][0].style.bold, Some(true));
        assert!(s.lines[0][0].style.link.is_some());
        assert!(!s.had_rejections);
    }

    #[test]
    fn clamps_oversize_slot() {
        let big = "a".repeat(MAX_RUN_TEXT_BYTES + 10);
        let s = sanitize_slot(&slot(vec![vec![run(&big)]]));
        assert!(s.had_rejections);
        assert!(s.lines[0][0].text.len() <= MAX_RUN_TEXT_BYTES);
    }

    #[test]
    fn clamps_line_and_run_counts() {
        let many_lines: Vec<Vec<StyledRun>> =
            (0..(MAX_SLOT_LINES + 5)).map(|_| vec![run("x")]).collect();
        let s = sanitize_slot(&slot(many_lines));
        assert_eq!(s.lines.len(), MAX_SLOT_LINES);
        assert!(s.had_rejections);

        let many_runs: Vec<StyledRun> = (0..(MAX_RUNS_PER_LINE + 3)).map(|_| run("y")).collect();
        let s2 = sanitize_slot(&slot(vec![many_runs]));
        assert_eq!(s2.lines[0].len(), MAX_RUNS_PER_LINE);
        assert!(s2.had_rejections);
    }

    #[test]
    fn hostile_fragments_across_generations_no_leak() {
        // Fragmented CSI + OSC 8 + DCS + private SGR sprinkled across runs.
        let hostile = vec![
            vec![
                run("\u{1b}[3"),
                run("1m RedThen"),
                run("\u{1b}]8;;https://ok\u{7}link\u{1b}]8;;\u{7}"),
            ],
            vec![run("\u{1b}[?2026h sync \u{1b}P+q\u{1b}\\ dcs")],
            vec![run("plain"), run("\u{0000}nul\u{0007}bel")],
        ];
        let s = sanitize_slot(&slot(hostile));
        let mut joined = String::new();
        for line in &s.lines {
            for r in line {
                joined.push_str(&r.text);
            }
        }
        assert!(!contains_control_bytes(joined.as_bytes()));
        assert!(joined.contains("RedThen"));
        assert!(joined.contains("link"));
        assert!(joined.contains("plain"));
        assert!(joined.contains("nulbel"));
        assert!(s.had_rejections);
    }

    #[test]
    fn plain_slot_has_no_rejections() {
        let s = sanitize_slot(&slot(vec![
            vec![run("hello"), run(" world")],
            vec![run("line two")],
        ]));
        assert_eq!(s.height, 2);
        assert!(!s.had_rejections);
        assert_eq!(s.lines[0][0].text, "hello");
    }

    #[test]
    fn caps_link_sizes() {
        let long_id = "i".repeat(MAX_LINK_ID_BYTES + 1);
        let mut styled = run("x");
        styled.style.link = Some(Hyperlink {
            id: Some(long_id),
            uri: "https://e.example".to_owned(),
        });
        let s = sanitize_slot(&slot(vec![vec![styled]]));
        assert!(s.lines[0][0].style.link.is_none());
        assert!(s.had_rejections);
    }
}
