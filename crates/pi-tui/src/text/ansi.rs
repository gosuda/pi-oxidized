//! ANSI / OSC / APC extraction and SGR + OSC-8 style tracking.

/// Terminator used by an OSC 8 hyperlink sequence.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Osc8Terminator {
    /// BEL (`\x07`).
    Bel,
    /// String Terminator (`ESC \\`).
    St,
}

impl Osc8Terminator {
    /// Return the sequence bytes for this terminator.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Bel => "\u{7}",
            Self::St => "\u{1b}\\",
        }
    }
}

/// Parsed OSC 8 hyperlink open.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ActiveHyperlink {
    /// Parameter portion between `ESC ] 8 ;` and the URL separator.
    pub params: String,
    /// Hyperlink URL (non-empty for open; empty means closed).
    pub url: String,
    /// Original terminator, preserved across wrap re-open.
    pub terminator: Osc8Terminator,
}

/// Extracted escape sequence at a byte index.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ExtractedAnsi<'a> {
    /// Full escape sequence including introducer and terminator.
    pub code: &'a str,
    /// Byte length of `code`.
    pub len: usize,
}

/// Extract a CSI / OSC / APC sequence starting at `pos`, or `None`.
///
/// Incomplete sequences return `None` (caller treats the ESC as a literal).
#[must_use]
pub fn extract_ansi_code(s: &str, pos: usize) -> Option<ExtractedAnsi<'_>> {
    let bytes = s.as_bytes();
    if pos >= bytes.len() || bytes[pos] != 0x1b {
        return None;
    }
    let next = *bytes.get(pos + 1)?;

    // CSI: ESC [ parameters (0x30-0x3f), intermediates (0x20-0x2f),
    // then exactly one final byte (0x40-0x7e).
    if next == b'[' {
        let mut j = pos + 2;
        let mut in_intermediates = false;
        while j < bytes.len() {
            match bytes[j] {
                0x30..=0x3f if !in_intermediates => j += 1,
                0x20..=0x2f => {
                    in_intermediates = true;
                    j += 1;
                }
                0x40..=0x7e => {
                    return Some(ExtractedAnsi {
                        code: &s[pos..=j],
                        len: j + 1 - pos,
                    });
                }
                _ => return None,
            }
        }
        return None;
    }

    // OSC: ESC ] ... BEL | ST
    if next == b']' {
        let mut j = pos + 2;
        while j < bytes.len() {
            if bytes[j] == 0x07 {
                return Some(ExtractedAnsi {
                    code: &s[pos..=j],
                    len: j + 1 - pos,
                });
            }
            if bytes[j] == 0x1b && bytes.get(j + 1) == Some(&b'\\') {
                return Some(ExtractedAnsi {
                    code: &s[pos..j + 2],
                    len: j + 2 - pos,
                });
            }
            j += 1;
        }
        return None;
    }

    // APC: ESC _ ... BEL | ST
    if next == b'_' {
        let mut j = pos + 2;
        while j < bytes.len() {
            if bytes[j] == 0x07 {
                return Some(ExtractedAnsi {
                    code: &s[pos..=j],
                    len: j + 1 - pos,
                });
            }
            if bytes[j] == 0x1b && bytes.get(j + 1) == Some(&b'\\') {
                return Some(ExtractedAnsi {
                    code: &s[pos..j + 2],
                    len: j + 2 - pos,
                });
            }
            j += 1;
        }
        return None;
    }

    None
}

/// Parse OSC 8 open/close.
///
/// - `None` if not OSC 8.
/// - `Some(None)` for OSC 8 close (empty URL).
/// - `Some(Some(link))` for OSC 8 open.
#[must_use]
pub fn parse_osc8_hyperlink(ansi_code: &str) -> Option<Option<ActiveHyperlink>> {
    if !ansi_code.starts_with("\u{1b}]8;") {
        return None;
    }
    let terminator = if ansi_code.ends_with('\u{7}') {
        Osc8Terminator::Bel
    } else if ansi_code.ends_with("\u{1b}\\") {
        Osc8Terminator::St
    } else {
        return None;
    };
    let body_end = ansi_code.len()
        - match terminator {
            Osc8Terminator::Bel => 1,
            Osc8Terminator::St => 2,
        };
    // Skip "\x1b]8;"
    let body = &ansi_code[4..body_end];
    let sep = body.find(';')?;
    let params = body[..sep].to_owned();
    let url = body[sep + 1..].to_owned();
    if url.is_empty() {
        return Some(None);
    }
    Some(Some(ActiveHyperlink {
        params,
        url,
        terminator,
    }))
}

/// Format an OSC 8 open sequence.
#[must_use]
pub fn format_osc8_hyperlink(link: &ActiveHyperlink) -> String {
    format!(
        "\u{1b}]8;{};{}{}",
        link.params,
        link.url,
        link.terminator.as_str()
    )
}

/// Format an OSC 8 close sequence with the original terminator.
#[must_use]
pub fn format_osc8_close(terminator: Osc8Terminator) -> String {
    format!("\u{1b}]8;;{}", terminator.as_str())
}

const BOLD: u16 = 1 << 0;
const DIM: u16 = 1 << 1;
const ITALIC: u16 = 1 << 2;
const UNDERLINE: u16 = 1 << 3;
const BLINK: u16 = 1 << 4;
const INVERSE: u16 = 1 << 5;
const HIDDEN: u16 = 1 << 6;
const STRIKETHROUGH: u16 = 1 << 7;

/// Tracks active SGR attributes and OSC 8 hyperlink state across wraps.
#[derive(Debug, Clone, Default)]
pub struct AnsiCodeTracker {
    attributes: u16,
    fg_color: Option<String>,
    bg_color: Option<String>,
    active_hyperlink: Option<ActiveHyperlink>,
}

impl AnsiCodeTracker {
    /// Create an empty tracker.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Process one escape sequence (SGR or OSC 8).
    pub fn process(&mut self, ansi_code: &str) {
        if let Some(hyperlink) = parse_osc8_hyperlink(ansi_code) {
            self.active_hyperlink = hyperlink;
            return;
        }
        if !ansi_code.ends_with('m') {
            return;
        }
        // Extract params between ESC [ and m
        let Some(rest) = ansi_code.strip_prefix("\u{1b}[") else {
            return;
        };
        let Some(params) = rest.strip_suffix('m') else {
            return;
        };
        if params.is_empty() || params == "0" {
            self.reset_sgr();
            return;
        }

        let parts: Vec<&str> = params.split(';').collect();
        let mut i = 0usize;
        while i < parts.len() {
            let code = parts[i].parse::<u32>().unwrap_or(u32::MAX);
            if code == 38 || code == 48 {
                if parts.get(i + 1) == Some(&"5") && parts.get(i + 2).is_some() {
                    let color = format!("{};5;{}", parts[i], parts[i + 2]);
                    if code == 38 {
                        self.fg_color = Some(color);
                    } else {
                        self.bg_color = Some(color);
                    }
                    i += 3;
                    continue;
                }
                if parts.get(i + 1) == Some(&"2")
                    && parts.get(i + 2).is_some()
                    && parts.get(i + 3).is_some()
                    && parts.get(i + 4).is_some()
                {
                    let color = format!(
                        "{};2;{};{};{}",
                        parts[i],
                        parts[i + 2],
                        parts[i + 3],
                        parts[i + 4]
                    );
                    if code == 38 {
                        self.fg_color = Some(color);
                    } else {
                        self.bg_color = Some(color);
                    }
                    i += 5;
                    continue;
                }
            }

            match code {
                0 => self.reset_sgr(),
                1 => self.set(BOLD),
                2 => self.set(DIM),
                3 => self.set(ITALIC),
                4 => self.set(UNDERLINE),
                5 => self.set(BLINK),
                7 => self.set(INVERSE),
                8 => self.set(HIDDEN),
                9 => self.set(STRIKETHROUGH),
                21 => self.unset(BOLD),
                22 => self.unset(BOLD | DIM),
                23 => self.unset(ITALIC),
                24 => self.unset(UNDERLINE),
                25 => self.unset(BLINK),
                27 => self.unset(INVERSE),
                28 => self.unset(HIDDEN),
                29 => self.unset(STRIKETHROUGH),
                39 => self.fg_color = None,
                49 => self.bg_color = None,
                30..=37 | 90..=97 => self.fg_color = Some(code.to_string()),
                40..=47 | 100..=107 => self.bg_color = Some(code.to_string()),
                _ => {}
            }
            i += 1;
        }
    }

    fn set(&mut self, flags: u16) {
        self.attributes |= flags;
    }

    fn unset(&mut self, flags: u16) {
        self.attributes &= !flags;
    }

    fn has(&self, flags: u16) -> bool {
        self.attributes & flags != 0
    }

    fn reset_sgr(&mut self) {
        self.attributes = 0;
        self.fg_color = None;
        self.bg_color = None;
        // SGR reset does not affect OSC 8 hyperlink state
    }

    /// Clear all state including hyperlink.
    pub fn clear(&mut self) {
        self.reset_sgr();
        self.active_hyperlink = None;
    }

    /// Emit active SGR + OSC 8 open for a continuation line.
    #[must_use]
    pub fn get_active_codes(&self) -> String {
        let mut codes: Vec<String> = Vec::new();
        for (flag, code) in [
            (BOLD, "1"),
            (DIM, "2"),
            (ITALIC, "3"),
            (UNDERLINE, "4"),
            (BLINK, "5"),
            (INVERSE, "7"),
            (HIDDEN, "8"),
            (STRIKETHROUGH, "9"),
        ] {
            if self.has(flag) {
                codes.push(code.into());
            }
        }
        if let Some(fg) = &self.fg_color {
            codes.push(fg.clone());
        }
        if let Some(bg) = &self.bg_color {
            codes.push(bg.clone());
        }

        let mut result = if codes.is_empty() {
            String::new()
        } else {
            format!("\u{1b}[{}m", codes.join(";"))
        };
        if let Some(link) = &self.active_hyperlink {
            result.push_str(&format_osc8_hyperlink(link));
        }
        result
    }

    /// Whether any SGR attribute or hyperlink is active.
    #[must_use]
    pub fn has_active_codes(&self) -> bool {
        self.attributes != 0
            || self.fg_color.is_some()
            || self.bg_color.is_some()
            || self.active_hyperlink.is_some()
    }

    /// Codes that must close at a soft line end (underline off + OSC 8 close).
    #[must_use]
    pub fn get_line_end_reset(&self) -> String {
        let mut result = String::new();
        if self.has(UNDERLINE) {
            result.push_str("\u{1b}[24m");
        }
        if let Some(link) = &self.active_hyperlink {
            result.push_str(&format_osc8_close(link.terminator));
        }
        result
    }
}

/// Walk `text` and feed every extractable escape into `tracker`.
pub fn update_tracker_from_text(text: &str, tracker: &mut AnsiCodeTracker) {
    let mut i = 0;
    while i < text.len() {
        if let Some(ansi) = extract_ansi_code(text, i) {
            tracker.process(ansi.code);
            i += ansi.len;
        } else {
            i += text[i..].chars().next().map_or(1, char::len_utf8);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::extract_ansi_code;
    use crate::text::visible_width;

    #[test]
    fn csi_accepts_every_standard_final_byte() {
        for final_byte in 0x40u8..=0x7e {
            let sequence = format!("\x1b[{}", char::from(final_byte));
            let extracted = extract_ansi_code(&sequence, 0);
            assert!(
                extracted.is_some(),
                "CSI final byte 0x{final_byte:02x} was rejected"
            );
            if let Some(extracted) = extracted {
                assert_eq!(extracted.len, sequence.len());
                assert_eq!(extracted.code, sequence);
            }
        }
    }

    #[test]
    fn private_mode_csi_has_zero_visible_width() {
        assert_eq!(visible_width("\x1b[?25lhello\x1b[?25h"), 5);
    }

    #[test]
    fn csi_rejects_parameter_bytes_after_intermediates() {
        assert!(extract_ansi_code("\x1b[ 1m", 0).is_none());
    }
}
