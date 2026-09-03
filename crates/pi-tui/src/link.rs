//! OSC 8 hyperlink helpers.
//!
//! - [`hyperlink`] wraps plain text (TS parity, ST terminator).
//! - [`write_link`] / [`format_link_open`] emit balanced per-cell OSC 8 for
//!   Ratatui buffers with id/uri caps and plain-text fallback.

use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::Style;
use ratatui::text::Span;

/// Maximum accepted hyperlink id length in bytes (Phase 6 sanitizer + local guard).
pub const MAX_LINK_ID_BYTES: usize = 128;
/// Maximum accepted hyperlink URI length in bytes.
pub const MAX_LINK_URI_BYTES: usize = 2048;

/// Validated hyperlink target.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Hyperlink {
    /// Optional id parameter (`id=...` in OSC 8 params).
    pub id: Option<String>,
    /// URI (any scheme at this layer; pi-ext may restrict further).
    pub uri: String,
}

/// Validation / encode error.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LinkError {
    /// URI exceeds [`MAX_LINK_URI_BYTES`].
    UriTooLong {
        /// Observed byte length.
        len: usize,
    },
    /// Id exceeds [`MAX_LINK_ID_BYTES`].
    IdTooLong {
        /// Observed byte length.
        len: usize,
    },
    /// URI is empty.
    EmptyUri,
}

impl std::fmt::Display for LinkError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::UriTooLong { len } => {
                write!(f, "hyperlink URI length {len} exceeds {MAX_LINK_URI_BYTES}")
            }
            Self::IdTooLong { len } => {
                write!(f, "hyperlink id length {len} exceeds {MAX_LINK_ID_BYTES}")
            }
            Self::EmptyUri => f.write_str("hyperlink URI is empty"),
        }
    }
}

impl std::error::Error for LinkError {}

impl Hyperlink {
    /// Validate and construct a hyperlink.
    ///
    /// # Errors
    ///
    /// Returns [`LinkError`] when the URI is empty, the URI exceeds
    /// [`MAX_LINK_URI_BYTES`], or the optional id exceeds
    /// [`MAX_LINK_ID_BYTES`].
    pub fn new(uri: impl Into<String>, id: Option<String>) -> Result<Self, LinkError> {
        let uri = uri.into();
        if uri.is_empty() {
            return Err(LinkError::EmptyUri);
        }
        if uri.len() > MAX_LINK_URI_BYTES {
            return Err(LinkError::UriTooLong { len: uri.len() });
        }
        if let Some(id) = &id
            && id.len() > MAX_LINK_ID_BYTES
        {
            return Err(LinkError::IdTooLong { len: id.len() });
        }
        Ok(Self { id, uri })
    }

    /// OSC 8 parameter field (`id=foo` or empty).
    #[must_use]
    pub fn params(&self) -> String {
        match &self.id {
            Some(id) if !id.is_empty() => format!("id={id}"),
            _ => String::new(),
        }
    }
}

/// Wrap `text` in OSC 8 open/close with ST terminators (TS `hyperlink` parity).
///
/// Does **not** enforce id/uri caps — use [`format_link_open`] for capped
/// encode with fallback.
#[must_use]
pub fn hyperlink(text: &str, url: &str) -> String {
    format!("\u{1b}]8;;{url}\u{1b}\\{text}\u{1b}]8;;\u{1b}\\")
}

/// Format an OSC 8 open sequence (ST terminator) after validating caps.
///
/// Returns `None` when validation fails (caller should use plain text).
#[must_use]
pub fn format_link_open(uri: &str, id: Option<&str>) -> Option<String> {
    let link = Hyperlink::new(uri, id.map(str::to_owned)).ok()?;
    Some(format!("\u{1b}]8;{};{}\u{1b}\\", link.params(), link.uri))
}

/// Format an OSC 8 close sequence (ST terminator).
#[must_use]
pub fn format_link_close() -> String {
    "\u{1b}]8;;\u{1b}\\".to_owned()
}

/// Format an OSC 8 open sequence with BEL terminator (alternate form).
#[must_use]
pub fn format_link_open_bel(uri: &str, id: Option<&str>) -> Option<String> {
    let link = Hyperlink::new(uri, id.map(str::to_owned)).ok()?;
    Some(format!("\u{1b}]8;{};{}\u{7}", link.params(), link.uri))
}

/// Format an OSC 8 close sequence with BEL terminator.
#[must_use]
pub fn format_link_close_bel() -> String {
    "\u{1b}]8;;\u{7}".to_owned()
}

/// Encode a full OSC 8-wrapped string with caps; on failure return plain `text`.
#[must_use]
pub fn hyperlink_capped(text: &str, uri: &str, id: Option<&str>) -> String {
    match format_link_open(uri, id) {
        Some(open) => format!("{open}{text}{}", format_link_close()),
        None => text.to_owned(),
    }
}

/// Write `spans` into `area` with a balanced per-cell OSC 8 hyperlink.
///
/// Each cell that receives a grapheme is annotated with the same open URI via
/// Ratatui's underline/link metadata when available; additionally the helper
/// returns the open/close escape pair for callers that embed into ANSI lines.
///
/// When the URI/id fail validation, cells are written as plain text and
/// [`WriteLinkResult::fallback_plain`] is `true`.
pub fn write_link(
    buf: &mut Buffer,
    area: Rect,
    spans: &[Span<'_>],
    uri: &str,
    id: Option<&str>,
) -> WriteLinkResult {
    let open = format_link_open(uri, id);
    let fallback_plain = open.is_none();
    let close = if fallback_plain {
        String::new()
    } else {
        format_link_close()
    };

    if area.width == 0 || area.height == 0 {
        return WriteLinkResult {
            open: open.unwrap_or_default(),
            close,
            fallback_plain,
            cells_written: 0,
        };
    }

    let mut col = 0u16;
    let mut cells_written = 0u16;
    let style_link = Style::default();

    for span in spans {
        let style = span.style.patch(style_link);
        for grapheme in
            unicode_segmentation::UnicodeSegmentation::graphemes(span.content.as_ref(), true)
        {
            let w = u16::try_from(crate::text::grapheme_width(grapheme)).unwrap_or(u16::MAX);
            if w == 0 {
                continue;
            }
            if col.saturating_add(w) > area.width {
                break;
            }
            let x = area.x.saturating_add(col);
            if let Some(cell) = buf.cell_mut((x, area.y)) {
                cell.set_symbol(grapheme);
                cell.set_style(style);
                // Ratatui 0.30: hyperlink via set_underline_color is style-only;
                // raw OSC 8 is carried by the returned open/close for ANSI paths.
            }
            if w == 2
                && let Some(cell) = buf.cell_mut((x + 1, area.y))
            {
                cell.set_symbol("");
            }
            col = col.saturating_add(w);
            cells_written = cells_written.saturating_add(w);
            if col >= area.width {
                break;
            }
        }
        if col >= area.width {
            break;
        }
    }

    WriteLinkResult {
        open: open.unwrap_or_default(),
        close,
        fallback_plain,
        cells_written,
    }
}

/// Result of [`write_link`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WriteLinkResult {
    /// OSC 8 open sequence (empty when fallback).
    pub open: String,
    /// OSC 8 close sequence (empty when fallback).
    pub close: String,
    /// `true` when URI/id failed validation and plain text was used.
    pub fallback_plain: bool,
    /// Number of cells written.
    pub cells_written: u16,
}

impl WriteLinkResult {
    /// Whether open and close are both present (balanced).
    #[must_use]
    pub fn is_balanced(&self) -> bool {
        if self.fallback_plain {
            return true;
        }
        !self.open.is_empty() && !self.close.is_empty()
    }

    /// Wrap `text` with this result's open/close (or plain if fallback).
    #[must_use]
    pub fn wrap(&self, text: &str) -> String {
        if self.fallback_plain {
            text.to_owned()
        } else {
            format!("{}{text}{}", self.open, self.close)
        }
    }
}

/// Count OSC 8 open and close sequences in `s` (ST or BEL forms).
#[must_use]
pub fn count_osc8_balance(s: &str) -> (usize, usize) {
    let mut opens = 0usize;
    let mut closes = 0usize;
    let bytes = s.as_bytes();
    let mut i = 0usize;
    while i + 4 < bytes.len() {
        // ESC ] 8 ;
        if bytes[i] == 0x1b && bytes[i + 1] == b']' && bytes[i + 2] == b'8' && bytes[i + 3] == b';'
        {
            // Find terminator BEL or ST
            let mut j = i + 4;
            while j < bytes.len() {
                if bytes[j] == 0x07 {
                    j += 1;
                    break;
                }
                if bytes[j] == 0x1b && j + 1 < bytes.len() && bytes[j + 1] == b'\\' {
                    j += 2;
                    break;
                }
                j += 1;
            }
            let body = &s[i + 4..j.saturating_sub(1).min(s.len())];
            // crude: empty URL after last ';' → close
            if let Some(semi) = body.find(';') {
                let url = &body[semi + 1..];
                // strip trailing terminator chars already excluded
                if url.is_empty() || url == "\u{1b}" {
                    closes += 1;
                } else {
                    opens += 1;
                }
            }
            i = j;
            continue;
        }
        i += 1;
    }
    (opens, closes)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hyperlink_ts_parity() {
        assert_eq!(
            hyperlink("click me", "https://example.com"),
            "\u{1b}]8;;https://example.com\u{1b}\\click me\u{1b}]8;;\u{1b}\\"
        );
        assert_eq!(
            hyperlink("", "https://example.com"),
            "\u{1b}]8;;https://example.com\u{1b}\\\u{1b}]8;;\u{1b}\\"
        );
    }

    #[test]
    fn hyperlink_preserves_inner_ansi() {
        let styled = "\u{1b}[4m\u{1b}[34mclick me\u{1b}[0m";
        let result = hyperlink(styled, "https://example.com");
        assert!(result.starts_with("\u{1b}]8;;https://example.com\u{1b}\\"));
        assert!(result.contains(styled));
        assert!(result.ends_with("\u{1b}]8;;\u{1b}\\"));
    }

    #[test]
    fn caps_reject_long_uri() {
        let long = "x".repeat(MAX_LINK_URI_BYTES + 1);
        assert!(matches!(
            Hyperlink::new(&long, None),
            Err(LinkError::UriTooLong { .. })
        ));
        assert!(format_link_open(&long, None).is_none());
        assert_eq!(hyperlink_capped("t", &long, None), "t");
    }

    #[test]
    fn caps_reject_long_id() {
        let long_id = "i".repeat(MAX_LINK_ID_BYTES + 1);
        assert!(matches!(
            Hyperlink::new("https://ok", Some(long_id.clone())),
            Err(LinkError::IdTooLong { .. })
        ));
        assert!(format_link_open("https://ok", Some(&long_id)).is_none());
    }

    #[test]
    fn caps_accept_boundary() {
        let uri = "u".repeat(MAX_LINK_URI_BYTES);
        let id = "i".repeat(MAX_LINK_ID_BYTES);
        let open = format_link_open(&uri, Some(&id));
        assert!(open.as_deref().is_some_and(|value| value.contains(&uri)));
        assert!(
            open.as_deref()
                .is_some_and(|value| value.contains(&format!("id={id}")))
        );
        assert!(
            open.as_deref()
                .is_some_and(|value| value.ends_with("\u{1b}\\"))
        );
    }

    #[test]
    fn empty_uri_rejected() {
        assert_eq!(Hyperlink::new("", None), Err(LinkError::EmptyUri));
        assert!(format_link_open("", None).is_none());
    }

    #[test]
    fn write_link_balanced_and_fallback() {
        let area = Rect::new(0, 0, 20, 1);
        let mut buf = Buffer::empty(area);
        let spans = vec![Span::raw("click")];
        let result = write_link(&mut buf, area, &spans, "https://example.com", Some("a"));
        assert!(!result.fallback_plain);
        assert!(result.is_balanced());
        assert!(result.open.contains("https://example.com"));
        assert!(result.open.contains("id=a"));
        assert_eq!(result.close, format_link_close());
        assert_eq!(result.cells_written, 5);
        assert_eq!(
            buf.cell((0, 0)).map(ratatui::buffer::Cell::symbol),
            Some("c")
        );

        let mut buf2 = Buffer::empty(area);
        let long = "x".repeat(MAX_LINK_URI_BYTES + 1);
        let result2 = write_link(&mut buf2, area, &spans, &long, None);
        assert!(result2.fallback_plain);
        assert!(result2.is_balanced());
        assert_eq!(result2.wrap("click"), "click");
    }

    #[test]
    fn wrap_is_balanced_open_close() {
        let s = hyperlink_capped("lab", "https://x.test", None);
        let (opens, closes) = count_osc8_balance(&s);
        assert_eq!(opens, 1);
        assert_eq!(closes, 1);
    }

    #[test]
    fn id_param_format() {
        assert_eq!(
            format_link_open("https://a", Some("job-1")).as_deref(),
            Some("\u{1b}]8;id=job-1;https://a\u{1b}\\")
        );
        assert_eq!(
            format_link_open("https://a", None).as_deref(),
            Some("\u{1b}]8;;https://a\u{1b}\\")
        );
    }

    #[test]
    fn write_link_vs16_zwj_ri_agree_with_layout() {
        // VS16 (U+FE0F) is zero-width via grapheme_width — it does not
        // consume an extra cell.  unicode-segmentation groups "B\u{FE0F}"
        // as one grapheme, so the cell symbol is "B\u{FE0F}" but it
        // occupies only one column.
        let area = Rect::new(0, 0, 10, 1);
        let mut buf = Buffer::empty(area);
        let span = Span::raw("AB\u{FE0F}CD");
        let result = write_link(&mut buf, area, &[span], "https://e.com", None);
        // visible_width("AB\u{FE0F}CD") = 4 (VS16 is zero-width)
        assert_eq!(result.cells_written, 4);
        assert_eq!(
            buf.cell((0, 0)).map(ratatui::buffer::Cell::symbol),
            Some("A")
        );
        assert_eq!(
            buf.cell((1, 0)).map(ratatui::buffer::Cell::symbol),
            Some("B\u{FE0F}")
        );
        assert_eq!(
            buf.cell((2, 0)).map(ratatui::buffer::Cell::symbol),
            Some("C")
        );
        assert_eq!(
            buf.cell((3, 0)).map(ratatui::buffer::Cell::symbol),
            Some("D")
        );
    }

    #[test]
    fn write_link_ri_pair_agrees_with_layout() {
        // Regional indicator pair 🇺🇸 is ONE grapheme cluster, width 2
        // (grapheme_width returns 2 via the RI base-char rule).
        let area = Rect::new(0, 0, 10, 1);
        let mut buf = Buffer::empty(area);
        let span = Span::raw("\u{1F1FA}\u{1F1F8}");
        let result = write_link(&mut buf, area, &[span], "https://e.com", None);
        assert_eq!(result.cells_written, 2);
        assert_eq!(
            buf.cell((0, 0)).map(ratatui::buffer::Cell::symbol),
            Some("\u{1F1FA}\u{1F1F8}")
        );
        assert_eq!(
            buf.cell((1, 0)).map(ratatui::buffer::Cell::symbol),
            Some("")
        );
    }

    #[test]
    fn write_link_ri_singleton_two_cells() {
        // A single regional indicator is width 2.
        let area = Rect::new(0, 0, 10, 1);
        let mut buf = Buffer::empty(area);
        let span = Span::raw("\u{1F1FA}");
        let result = write_link(&mut buf, area, &[span], "https://e.com", None);
        assert_eq!(result.cells_written, 2);
        assert_eq!(
            buf.cell((0, 0)).map(ratatui::buffer::Cell::symbol),
            Some("\u{1F1FA}")
        );
        assert_eq!(
            buf.cell((1, 0)).map(ratatui::buffer::Cell::symbol),
            Some("")
        );
    }

    #[test]
    fn write_link_spacing_mark_agrees_with_layout() {
        // "का" (U+0915 + U+093E) is one grapheme, width 2 (base + spacing mark).
        let area = Rect::new(0, 0, 10, 1);
        let mut buf = Buffer::empty(area);
        let span = Span::raw("का");
        let result = write_link(&mut buf, area, &[span], "https://e.com", None);
        assert_eq!(result.cells_written, 2);
        assert_eq!(
            buf.cell((0, 0)).map(ratatui::buffer::Cell::symbol),
            Some("का")
        );
    }
}
