//! Terminal text primitives: width, wrap, truncate, slice, and ANSI-aware surgery.
//!
//! Ports the observable semantics of `.references/pi/packages/tui/src/utils.ts`
//! (plus `compositeLineAt` / `CURSOR_MARKER` helpers from `tui.ts`).

mod ansi;
mod slice;
mod width;
mod wrap;

pub use ansi::{
    ActiveHyperlink, AnsiCodeTracker, ExtractedAnsi, Osc8Terminator, extract_ansi_code,
    format_osc8_close, format_osc8_hyperlink, parse_osc8_hyperlink,
};
pub use slice::{
    CURSOR_MARKER, ExtractedSegments, SEGMENT_RESET, SliceWithWidth, TRUNCATION_MARKER,
    composite_line_at, extract_cursor_marker, extract_segments, find_cursor_marker, is_image_line,
    slice_by_column, slice_with_width, strip_cursor_marker, truncate_to_width,
    truncate_with_marker,
};
pub use width::{
    PUNCTUATION, apply_background_to_line, cjk_break_grapheme, grapheme_width, is_punctuation_char,
    is_whitespace_char, normalize_terminal_output, visible_width,
};
pub use wrap::{
    is_partial_closing_fence_line, strip_trailing_partial_closing_fence, wrap_text_with_ansi,
};

#[cfg(test)]
mod tests;
